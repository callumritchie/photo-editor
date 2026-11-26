import SwiftUI

/// Crop rectangle in fractions (0...1) of the corrected frame.
struct CropRect: Equatable {
    var left = 0.0, top = 0.0, right = 1.0, bottom = 1.0

    static let full = CropRect()
    var width: Double { right - left }
    var height: Double { bottom - top }
}

/// Aspect-ratio presets for the crop tool. `nil` ratio = free; 0 = the photo's own ratio.
struct CropAspect: Hashable, Identifiable {
    let label: String
    let ratio: Double?
    var id: String { label }

    static let presets = [
        CropAspect(label: "Free", ratio: nil),
        CropAspect(label: "Original", ratio: 0),
        CropAspect(label: "1 : 1", ratio: 1),
        CropAspect(label: "4 : 5", ratio: 4.0 / 5.0),
        CropAspect(label: "3 : 2", ratio: 3.0 / 2.0),
        CropAspect(label: "16 : 9", ratio: 16.0 / 9.0),
    ]
}

extension DevelopSettings {
    var crop: CropRect {
        get {
            CropRect(left: number("crop.left") ?? 0, top: number("crop.top") ?? 0,
                     right: number("crop.right") ?? 1, bottom: number("crop.bottom") ?? 1)
        }
        set {
            setNumber("crop.left", newValue.left)
            setNumber("crop.top", newValue.top)
            setNumber("crop.right", newValue.right)
            setNumber("crop.bottom", newValue.bottom)
        }
    }

    var cropAngle: Double {
        get { number("crop.angle") ?? 0 }
        set { setNumber("crop.angle", newValue) }
    }
}

/// Largest centred crop (same aspect as the frame) inside a frame of aspect `aspect` rotated by `angle`
/// degrees, as a fraction of the frame (mirrors `geometry::straighten_fit` in the core).
func straightenFit(aspect: Double, angle: Double) -> Double {
    let (w, h) = (aspect, 1.0)
    let s = abs(sin(angle * .pi / 180)), c = cos(angle * .pi / 180)
    return min(w / (w * c + h * s), h / (w * s + h * c))
}

/// Draggable crop rectangle drawn over the full (uncropped) image.
struct CropOverlay: View {
    @Binding var rect: CropRect
    /// Width / height of the crop in pixels, or nil for free.
    let aspect: Double?
    /// Frame width / height, to convert a pixel aspect into fractions.
    let frameAspect: Double
    let onCommit: () -> Void

    @State private var start: CropRect?

    var body: some View {
        GeometryReader { geo in
            let size = geo.size
            let r = CGRect(x: rect.left * size.width, y: rect.top * size.height,
                           width: rect.width * size.width, height: rect.height * size.height)
            ZStack(alignment: .topLeading) {
                // Dim outside the crop.
                Path { p in
                    p.addRect(CGRect(origin: .zero, size: size))
                    p.addRect(r)
                }
                .fill(Color.black.opacity(0.55), style: FillStyle(eoFill: true))
                // Thirds and border.
                Path { p in
                    for k in 1...2 {
                        let x = r.minX + r.width * Double(k) / 3, y = r.minY + r.height * Double(k) / 3
                        p.move(to: CGPoint(x: x, y: r.minY)); p.addLine(to: CGPoint(x: x, y: r.maxY))
                        p.move(to: CGPoint(x: r.minX, y: y)); p.addLine(to: CGPoint(x: r.maxX, y: y))
                    }
                }
                .stroke(Color.white.opacity(0.45), lineWidth: 0.5)
                Rectangle().path(in: r).stroke(Color.white, lineWidth: 1)
                // Move by dragging inside.
                Rectangle().fill(Color.white.opacity(0.001))
                    .frame(width: r.width, height: r.height)
                    .offset(x: r.minX, y: r.minY)
                    .gesture(drag(size) { s, dx, dy in
                        let dx = min(max(dx, -s.left), 1 - s.right), dy = min(max(dy, -s.top), 1 - s.bottom)
                        return CropRect(left: s.left + dx, top: s.top + dy, right: s.right + dx, bottom: s.bottom + dy)
                    })
                ForEach(0..<4, id: \.self) { corner in
                    let (cx, cy) = (corner % 2 == 0 ? r.minX : r.maxX, corner < 2 ? r.minY : r.maxY)
                    RoundedRectangle(cornerRadius: 2).fill(Color.white)
                        .frame(width: 14, height: 14)
                        .offset(x: cx - 7, y: cy - 7)
                        .gesture(drag(size) { s, dx, dy in resize(s, corner: corner, dx: dx, dy: dy) })
                }
            }
        }
    }

    private func drag(_ size: CGSize, _ apply: @escaping (CropRect, Double, Double) -> CropRect) -> some Gesture {
        DragGesture(minimumDistance: 0)
            .onChanged { g in
                let s = start ?? rect
                if start == nil { start = rect }
                rect = apply(s, g.translation.width / size.width, g.translation.height / size.height)
            }
            .onEnded { _ in start = nil; onCommit() }
    }

    /// Move one corner; with a fixed aspect the height follows the width.
    private func resize(_ s: CropRect, corner: Int, dx: Double, dy: Double) -> CropRect {
        let minSize = 0.05
        var r = s
        if corner % 2 == 0 { r.left = min(max(s.left + dx, 0), s.right - minSize) }
        else { r.right = max(min(s.right + dx, 1), s.left + minSize) }
        if corner < 2 { r.top = min(max(s.top + dy, 0), s.bottom - minSize) }
        else { r.bottom = max(min(s.bottom + dy, 1), s.top + minSize) }
        if let a = aspect, a > 0 {
            // Height in fractions for the chosen pixel aspect: (w * frameAspect) / h = a.
            var h = r.width * frameAspect / a
            if corner < 2 {
                h = min(h, s.bottom)
                r.top = s.bottom - h
            } else {
                h = min(h, 1 - s.top)
                r.bottom = s.top + h
            }
            // If the height was limited, shrink the width to match.
            let w = h * a / frameAspect
            if corner % 2 == 0 { r.left = s.right - w } else { r.right = s.left + w }
        }
        return r
    }
}

/// Fit a rectangle of pixel aspect `a` centred in the current crop's centre, as large as possible.
func fitAspect(_ rect: CropRect, aspect a: Double, frameAspect fa: Double) -> CropRect {
    let cx = (rect.left + rect.right) / 2, cy = (rect.top + rect.bottom) / 2
    var w = 1.0, h = w * fa / a
    if h > 1 { h = 1; w = h * a / fa }
    let left = min(max(cx - w / 2, 0), 1 - w), top = min(max(cy - h / 2, 0), 1 - h)
    return CropRect(left: left, top: top, right: left + w, bottom: top + h)
}
