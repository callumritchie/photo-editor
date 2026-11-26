import AppKit
import CPeFFI
import SwiftUI

extension PreviewImage {
    /// Full-resolution output size (after crop and geometry).
    func outputSize(_ settings: DevelopSettings) -> CGSize {
        var w: UInt32 = 0, h: UInt32 = 0
        _ = pe_image_output_size(handlePointer, settings.json, &w, &h)
        return CGSize(width: Int(w), height: Int(h))
    }

    /// The photo's recorded lens and its matched profile, if any.
    func lensProfile() -> (lens: String?, profile: String?) {
        struct Info: Decodable { let lens: String?; let profile: String? }
        guard let p = pe_image_lens_profile(handlePointer) else { return (nil, nil) }
        defer { pe_string_free(p) }
        let info = try? JSONDecoder().decode(Info.self, from: Data(bytes: p, count: strlen(p)))
        return (info?.lens, info?.profile)
    }

    /// The full-resolution output rectangle at 1:1.
    func renderRegion(_ settings: DevelopSettings, _ r: CGRect) -> NSImage? {
        var w: UInt32 = 0, h: UInt32 = 0
        guard let p = pe_image_render_region(handlePointer, settings.json, UInt32(r.minX), UInt32(r.minY),
                                             UInt32(r.width), UInt32(r.height), &w, &h) else { return nil }
        defer { pe_bytes_free(p, Int(w) * Int(h) * 4) }
        return Core.imageFromRGBA(p, Int(w), Int(h))
    }
}

/// 1:1 view of the full-resolution photo, panned by dragging; a click returns to the fitted view.
struct ZoomView: View {
    @ObservedObject var model: DevelopModel
    @State private var dragStart: CGPoint?
    @State private var moved = false

    var body: some View {
        GeometryReader { geo in
            let scale = NSScreen.main?.backingScaleFactor ?? 2
            ZStack {
                Color.black.opacity(0.92)
                if let img = model.zoomImage {
                    Image(nsImage: img)
                        .interpolation(.none)
                        .resizable()
                        .frame(width: img.size.width / scale, height: img.size.height / scale)
                }
                if model.zoomImage == nil { ProgressView() }
            }
            .frame(width: geo.size.width, height: geo.size.height)
            .contentShape(Rectangle())
            .gesture(DragGesture(minimumDistance: 0)
                .onChanged { g in
                    let start = dragStart ?? model.zoomCenter
                    if dragStart == nil { dragStart = start; moved = false }
                    if abs(g.translation.width) + abs(g.translation.height) > 3 { moved = true }
                    let full = model.fullSize
                    guard full.width > 0, full.height > 0 else { return }
                    model.zoomCenter = CGPoint(x: start.x - g.translation.width * scale / full.width,
                                               y: start.y - g.translation.height * scale / full.height)
                    model.requestZoomRender()
                }
                .onEnded { _ in
                    dragStart = nil
                    if !moved { model.setZoom(false) }
                })
            .onAppear { model.viewport = CGSize(width: geo.size.width * scale, height: geo.size.height * scale) }
            .onChange(of: geo.size) { _, s in
                model.viewport = CGSize(width: s.width * scale, height: s.height * scale)
                model.requestZoomRender()
            }
        }
    }
}

extension DevelopModel {
    /// Zoom to 1:1 centred on `at` (fractions of the photo), or back to the fitted view.
    func setZoom(_ on: Bool, at: CGPoint? = nil) {
        if let at { zoomCenter = at }
        zoomed = on
        if on {
            zoomImage = nil
            requestZoomRender()
        }
    }
}
