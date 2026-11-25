import CPeFFI
import Foundation

enum ExportFormat: String, CaseIterable, Identifiable, Hashable {
    case jpeg, png, tiff, original
    var id: String { rawValue }
    var label: String {
        switch self {
        case .jpeg: "JPEG"
        case .png: "PNG"
        case .tiff: "TIFF"
        case .original: "Original file"
        }
    }
}

enum ResizeMode: String, CaseIterable, Identifiable, Hashable {
    case original, longEdge, shortEdge, width, height, fit, megapixels, percent
    var id: String { rawValue }
    var label: String {
        switch self {
        case .original: "Full size"
        case .longEdge: "Long edge"
        case .shortEdge: "Short edge"
        case .width: "Width"
        case .height: "Height"
        case .fit: "Fit within"
        case .megapixels: "Megapixels"
        case .percent: "Percentage"
        }
    }
}

/// Flat, form-friendly export settings. Encodes to and decodes from the core's `ExportSettings` JSON.
struct ExportOptions: Codable, Hashable {
    var destination = FileManager.default.urls(for: .picturesDirectory, in: .userDomainMask).first!
        .appendingPathComponent("Exports").path
    var subfolder = ""
    var fileNaming = "{name}"
    var startNumber = 1
    var collision = "rename"
    var format: ExportFormat = .jpeg
    var quality = 90
    var sixteenBit = false
    var tiffCompression = "lzw"
    var colorSpace = "srgb"
    var resizeMode: ResizeMode = .original
    var pixels = 2048
    var width = 2048
    var height = 2048
    var megapixels = 12.0
    var percent = 50.0
    var dontEnlarge = true
    var sharpen = true
    var sharpenAmount = "standard"
    var sharpenMedium = "screen"
    var ppi = 240
    var metadata = "all"
    var artist = ""
    var copyright = ""

    init() {}

    private struct K: CodingKey {
        var stringValue: String
        var intValue: Int? { nil }
        init(_ s: String) { stringValue = s }
        init?(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { nil }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: K.self)
        try c.encode(destination, forKey: K("destination"))
        let sub = subfolder.trimmingCharacters(in: .whitespaces)
        try c.encode(sub.isEmpty ? nil : sub, forKey: K("subfolder"))
        try c.encode(fileNaming, forKey: K("fileNaming"))
        try c.encode(startNumber, forKey: K("startNumber"))
        try c.encode(collision, forKey: K("collision"))
        var f = c.nestedContainer(keyedBy: K.self, forKey: K("format"))
        try f.encode(format.rawValue, forKey: K("type"))
        switch format {
        case .jpeg: try f.encode(quality, forKey: K("quality"))
        case .png: try f.encode(sixteenBit, forKey: K("sixteenBit"))
        case .tiff:
            try f.encode(sixteenBit, forKey: K("sixteenBit"))
            try f.encode(tiffCompression, forKey: K("compression"))
        case .original: break
        }
        try c.encode(colorSpace, forKey: K("colorSpace"))
        var r = c.nestedContainer(keyedBy: K.self, forKey: K("resize"))
        try r.encode(resizeMode.rawValue, forKey: K("mode"))
        switch resizeMode {
        case .original: break
        case .longEdge, .shortEdge, .width, .height: try r.encode(pixels, forKey: K("pixels"))
        case .fit:
            try r.encode(width, forKey: K("width"))
            try r.encode(height, forKey: K("height"))
        case .megapixels: try r.encode(megapixels, forKey: K("megapixels"))
        case .percent: try r.encode(percent, forKey: K("percent"))
        }
        try c.encode(dontEnlarge, forKey: K("dontEnlarge"))
        if sharpen {
            var s = c.nestedContainer(keyedBy: K.self, forKey: K("sharpen"))
            try s.encode(sharpenAmount, forKey: K("amount"))
            try s.encode(sharpenMedium, forKey: K("medium"))
        } else {
            try c.encodeNil(forKey: K("sharpen"))
        }
        try c.encode(ppi, forKey: K("ppi"))
        try c.encode(metadata, forKey: K("metadata"))
        try c.encode(artist.isEmpty ? nil : artist, forKey: K("artist"))
        try c.encode(copyright.isEmpty ? nil : copyright, forKey: K("copyright"))
    }

    init(from decoder: Decoder) throws {
        self.init()
        let c = try decoder.container(keyedBy: K.self)
        func get<T: Decodable>(_ key: String, _ into: inout T, in cont: KeyedDecodingContainer<K>? = nil) {
            if let v = try? (cont ?? c).decodeIfPresent(T.self, forKey: K(key)) { into = v }
        }
        get("destination", &destination)
        var sub: String? = nil
        get("subfolder", &sub)
        subfolder = sub ?? ""
        get("fileNaming", &fileNaming)
        get("startNumber", &startNumber)
        get("collision", &collision)
        if let f = try? c.nestedContainer(keyedBy: K.self, forKey: K("format")) {
            var t = "jpeg"
            get("type", &t, in: f)
            format = ExportFormat(rawValue: t) ?? .jpeg
            get("quality", &quality, in: f)
            get("sixteenBit", &sixteenBit, in: f)
            get("compression", &tiffCompression, in: f)
        }
        get("colorSpace", &colorSpace)
        if let r = try? c.nestedContainer(keyedBy: K.self, forKey: K("resize")) {
            var m = "original"
            get("mode", &m, in: r)
            resizeMode = ResizeMode(rawValue: m) ?? .original
            get("pixels", &pixels, in: r)
            get("width", &width, in: r)
            get("height", &height, in: r)
            get("megapixels", &megapixels, in: r)
            get("percent", &percent, in: r)
        }
        get("dontEnlarge", &dontEnlarge)
        if let s = try? c.nestedContainer(keyedBy: K.self, forKey: K("sharpen")) {
            sharpen = true
            get("amount", &sharpenAmount, in: s)
            get("medium", &sharpenMedium, in: s)
        } else {
            sharpen = false
        }
        get("ppi", &ppi)
        get("metadata", &metadata)
        var a: String? = nil, cp: String? = nil
        get("artist", &a)
        get("copyright", &cp)
        artist = a ?? ""
        copyright = cp ?? ""
    }

    var json: String {
        String(data: (try? JSONEncoder().encode(self)) ?? Data("{}".utf8), encoding: .utf8) ?? "{}"
    }

    /// What the template produces for a sample photo, for the form's live preview.
    func sampleName(for item: ImageItem?) -> String {
        let name = item.map { URL(fileURLWithPath: $0.path).deletingPathExtension().lastPathComponent } ?? "IMG_0001"
        var s = fileNaming.replacingOccurrences(of: "{name}", with: name)
        s = s.replacingOccurrences(of: "{seq}", with: String(startNumber))
        for n in 2...6 {
            s = s.replacingOccurrences(of: "{seq:\(n)}", with: String(format: "%0\(n)d", startNumber))
        }
        let ext: String
        switch format {
        case .jpeg: ext = "jpg"
        case .png: ext = "png"
        case .tiff: ext = "tif"
        case .original: ext = item.map { URL(fileURLWithPath: $0.path).pathExtension.lowercased() } ?? "raw"
        }
        return s + "." + ext
    }
}

struct ExportPresetItem: Decodable, Hashable, Identifiable {
    let id: Int64
    let uuid: String
    let name: String
    let settings: ExportOptions
}

struct ExportReport: Decodable {
    let written: [String]
    let skipped: [String]
    let failed: [[String]]
}

private struct ExportErrorReply: Decodable { let error: String }

private func decodeReply<T: Decodable>(_ p: UnsafeMutablePointer<CChar>?) throws -> T {
    guard let p else { throw CoreError.message("no response from core") }
    defer { pe_string_free(p) }
    let data = Data(bytes: p, count: strlen(p))
    if let e = try? JSONDecoder().decode(ExportErrorReply.self, from: data) { throw CoreError.message(e.error) }
    return try JSONDecoder().decode(T.self, from: data)
}

private final class ExportProgressBox {
    let callback: (Int, Int) -> Void
    init(_ callback: @escaping (Int, Int) -> Void) { self.callback = callback }
}

extension Core {
    func export(_ ids: [Int64], options: ExportOptions, progress: @escaping (Int, Int) -> Void) throws -> ExportReport {
        let box = Unmanaged.passRetained(ExportProgressBox(progress)).toOpaque()
        defer { Unmanaged<ExportProgressBox>.fromOpaque(box).release() }
        let result = ids.withUnsafeBufferPointer { buf in
            pe_catalog_export_json(handlePointer, buf.baseAddress, ids.count, options.json, { ctx, done, total in
                guard let ctx else { return }
                Unmanaged<ExportProgressBox>.fromOpaque(ctx).takeUnretainedValue().callback(Int(done), Int(total))
            }, box)
        }
        return try decodeReply(result)
    }

    func exportPresets() throws -> [ExportPresetItem] { try decodeReply(pe_catalog_export_presets_json(handlePointer)) }

    @discardableResult
    func saveExportPreset(name: String, options: ExportOptions) throws -> ExportPresetItem {
        try decodeReply(pe_catalog_save_export_preset_json(handlePointer, name, options.json))
    }

    func deleteExportPreset(_ id: Int64) { _ = pe_catalog_delete_export_preset(handlePointer, id) }
}
