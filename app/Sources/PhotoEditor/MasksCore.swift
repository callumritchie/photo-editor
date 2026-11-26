// Mask models (mirroring pe_pipeline::masks) and the core / Vision calls behind the masking tools.

import AppKit
import CPeFFI
import Foundation
import Vision

struct StrokeModel: Codable, Hashable {
    var points: [[Double]]
    var size: Double
    var feather: Double
    var flow: Double
    var erase: Bool
}

enum ShapeModel: Hashable {
    case linear(x0: Double, y0: Double, x1: Double, y1: Double)
    case radial(cx: Double, cy: Double, rx: Double, ry: Double, angle: Double, feather: Double)
    case brush(strokes: [StrokeModel])
    case luminance(lo: Double, hi: Double, smoothness: Double)
    case color(colors: [[Double]], tolerance: Double)
    case raster(rasterId: String, label: String)

    var title: String {
        switch self {
        case .linear: return "Linear Gradient"
        case .radial: return "Radial Gradient"
        case .brush: return "Brush"
        case .luminance: return "Luminance Range"
        case .color: return "Colour Range"
        case .raster(_, let label): return label
        }
    }

    var icon: String {
        switch self {
        case .linear: return "square.bottomhalf.filled"
        case .radial: return "circle.dashed.inset.filled"
        case .brush: return "paintbrush.pointed"
        case .luminance: return "sun.max"
        case .color: return "eyedropper"
        case .raster: return "person.crop.rectangle"
        }
    }
}

extension ShapeModel: Codable {
    private enum K: String, CodingKey {
        case type, x0, y0, x1, y1, cx, cy, rx, ry, angle, feather, strokes, lo, hi, smoothness, colors, tolerance, rasterId, label
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        func d(_ k: K, _ def: Double = 0) -> Double { (try? c.decode(Double.self, forKey: k)) ?? def }
        switch try c.decode(String.self, forKey: .type) {
        case "linear": self = .linear(x0: d(.x0), y0: d(.y0), x1: d(.x1), y1: d(.y1))
        case "radial": self = .radial(cx: d(.cx), cy: d(.cy), rx: d(.rx), ry: d(.ry), angle: d(.angle), feather: d(.feather, 0.5))
        case "brush": self = .brush(strokes: (try? c.decode([StrokeModel].self, forKey: .strokes)) ?? [])
        case "luminance": self = .luminance(lo: d(.lo), hi: d(.hi, 1), smoothness: d(.smoothness, 0.5))
        case "color": self = .color(colors: (try? c.decode([[Double]].self, forKey: .colors)) ?? [], tolerance: d(.tolerance, 50))
        default:
            self = .raster(rasterId: (try? c.decode(String.self, forKey: .rasterId)) ?? "",
                           label: (try? c.decode(String.self, forKey: .label)) ?? "Selection")
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: K.self)
        switch self {
        case .linear(let x0, let y0, let x1, let y1):
            try c.encode("linear", forKey: .type)
            try c.encode(x0, forKey: .x0); try c.encode(y0, forKey: .y0)
            try c.encode(x1, forKey: .x1); try c.encode(y1, forKey: .y1)
        case .radial(let cx, let cy, let rx, let ry, let angle, let feather):
            try c.encode("radial", forKey: .type)
            try c.encode(cx, forKey: .cx); try c.encode(cy, forKey: .cy)
            try c.encode(rx, forKey: .rx); try c.encode(ry, forKey: .ry)
            try c.encode(angle, forKey: .angle); try c.encode(feather, forKey: .feather)
        case .brush(let strokes):
            try c.encode("brush", forKey: .type)
            try c.encode(strokes, forKey: .strokes)
        case .luminance(let lo, let hi, let s):
            try c.encode("luminance", forKey: .type)
            try c.encode(lo, forKey: .lo); try c.encode(hi, forKey: .hi); try c.encode(s, forKey: .smoothness)
        case .color(let colors, let tol):
            try c.encode("color", forKey: .type)
            try c.encode(colors, forKey: .colors); try c.encode(tol, forKey: .tolerance)
        case .raster(let id, let label):
            try c.encode("raster", forKey: .type)
            try c.encode(id, forKey: .rasterId)
            try c.encode(label, forKey: .label)  // ignored by the core
        }
    }
}

struct ComponentModel: Codable, Hashable {
    var mode = "add"
    var invert = false
    var shape: ShapeModel
}

struct MaskModel: Codable, Hashable, Identifiable {
    var id = UUID().uuidString
    var name = ""
    var enabled = true
    var amount = 100.0
    var components: [ComponentModel]
    var adjust: [String: Double] = [:]
}

extension DevelopSettings {
    var masks: [MaskModel] {
        get {
            guard let v = value[["masks"][...]], let data = try? JSONEncoder().encode(v) else { return [] }
            return (try? JSONDecoder().decode([MaskModel].self, from: data)) ?? []
        }
        set {
            guard let data = try? JSONEncoder().encode(newValue),
                  let v = try? JSONDecoder().decode(JSONValue.self, from: data) else { return }
            value[["masks"][...]] = v
        }
    }

    /// The same settings with geometry neutralised (masks are edited on the uncropped frame).
    var withoutGeometry: DevelopSettings {
        var s = self
        s.crop = .full
        s.cropAngle = 0
        s.value[["transform"][...]] = .object([:])  // the core fills in defaults (null would be rejected)
        for k in ["distortion", "caRedCyan", "caBlueYellow"] { s.setNumber("lens.\(k)", 0) }
        return s
    }
}

/// An 8-bit grey raster.
struct GreyRaster {
    let width: Int
    let height: Int
    let bytes: [UInt8]
}

extension PreviewImage {
    func attachRasters(core: Core, imageId: Int64) {
        _ = pe_image_attach_rasters(handlePointer, core.handlePointer, imageId)
    }

    func addRaster(id: String, _ r: GreyRaster) {
        r.bytes.withUnsafeBufferPointer { b in
            _ = pe_image_add_raster(handlePointer, id, UInt32(r.width), UInt32(r.height), b.baseAddress, b.count)
        }
    }

    func selectSky() -> GreyRaster? {
        var w: UInt32 = 0, h: UInt32 = 0
        guard let p = pe_image_select_sky(handlePointer, &w, &h) else { return nil }
        let n = Int(w) * Int(h)
        defer { pe_bytes_free(p, n) }
        return GreyRaster(width: Int(w), height: Int(h), bytes: Array(UnsafeBufferPointer(start: p, count: n)))
    }

    /// Red overlay (alpha = mask value) of mask `index` over the uncropped frame.
    func maskOverlay(_ settings: DevelopSettings, index: Int) -> NSImage? {
        var w: UInt32 = 0, h: UInt32 = 0
        guard let p = pe_image_mask_overlay(handlePointer, settings.json, UInt32(index), &w, &h) else { return nil }
        let n = Int(w) * Int(h)
        defer { pe_bytes_free(p, n) }
        var rgba = [UInt8](repeating: 0, count: n * 4)
        for i in 0..<n {
            let a = p[i]
            // Premultiplied red.
            rgba[i * 4] = UInt8(min(255, Int(a) * 230 / 255))
            rgba[i * 4 + 3] = UInt8(Int(a) * 140 / 255)
        }
        return rgba.withUnsafeMutableBufferPointer { buf -> NSImage? in
            guard let ctx = CGContext(data: buf.baseAddress, width: Int(w), height: Int(h), bitsPerComponent: 8,
                                      bytesPerRow: Int(w) * 4, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue),
                  let cg = ctx.makeImage() else { return nil }
            return NSImage(cgImage: cg, size: NSSize(width: Int(w), height: Int(h)))
        }
    }

    func pickColor(_ settings: DevelopSettings, fx: Double, fy: Double) -> [Double]? {
        var out = [Float](repeating: 0, count: 3)
        let ok = out.withUnsafeMutableBufferPointer { pe_image_pick_color(handlePointer, settings.json, Float(fx), Float(fy), $0.baseAddress) }
        return ok ? out.map(Double.init) : nil
    }

    /// Select Subject with Apple's Vision framework on the uncropped frame.
    func selectSubject(_ settings: DevelopSettings) -> GreyRaster? {
        guard let image = render(settings.withoutGeometry),
              let cg = image.cgImage(forProposedRect: nil, context: nil, hints: nil) else { return nil }
        let request = VNGenerateForegroundInstanceMaskRequest()
        let handler = VNImageRequestHandler(cgImage: cg)
        guard (try? handler.perform([request])) != nil, let obs = request.results?.first,
              let buffer = try? obs.generateScaledMaskForImage(forInstances: obs.allInstances, from: handler) else { return nil }
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        let (bw, bh) = (CVPixelBufferGetWidth(buffer), CVPixelBufferGetHeight(buffer))
        let row = CVPixelBufferGetBytesPerRow(buffer)
        guard let base = CVPixelBufferGetBaseAddress(buffer) else { return nil }
        // Area-average down to at most 512 on the long edge.
        let scale = min(1.0, 512.0 / Double(max(bw, bh)))
        let (w, h) = (max(1, Int(Double(bw) * scale)), max(1, Int(Double(bh) * scale)))
        var out = [UInt8](repeating: 0, count: w * h)
        for y in 0..<h {
            let y0 = y * bh / h, y1 = max(y0 + 1, (y + 1) * bh / h)
            for x in 0..<w {
                let x0 = x * bw / w, x1 = max(x0 + 1, (x + 1) * bw / w)
                var s: Float = 0
                for yy in y0..<y1 {
                    let p = base.advanced(by: yy * row).assumingMemoryBound(to: Float.self)
                    for xx in x0..<x1 { s += p[xx] }
                }
                out[y * w + x] = UInt8(max(0, min(255, s / Float((y1 - y0) * (x1 - x0)) * 255 + 0.5)))
            }
        }
        return GreyRaster(width: w, height: h, bytes: out)
    }
}

extension Core {
    /// Store a raster for a photo in the library; returns its id.
    func saveRaster(imageId: Int64, _ r: GreyRaster) throws -> String {
        struct Saved: Decodable { let id: String }
        struct Failure: Decodable { let error: String }
        let p = r.bytes.withUnsafeBufferPointer {
            pe_catalog_save_raster(handlePointer, imageId, UInt32(r.width), UInt32(r.height), $0.baseAddress, $0.count)
        }
        guard let p else { throw CoreError.message("no response from core") }
        defer { pe_string_free(p) }
        let data = Data(bytes: p, count: strlen(p))
        if let f = try? JSONDecoder().decode(Failure.self, from: data) { throw CoreError.message(f.error) }
        return try JSONDecoder().decode(Saved.self, from: data).id
    }
}
