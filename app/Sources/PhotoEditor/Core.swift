// Swift wrapper around the Rust core's C interface (crates/ffi). All calls are thread-safe and may
// block, so the UI calls them from background tasks.

import AppKit
import CPeFFI
import Foundation

struct ImageItem: Codable, Identifiable, Hashable {
    let id: Int64
    let uuid: String
    let folderId: Int64
    let path: String
    let name: String
    let captureTime: String?
    var rating: Int
    var flag: String  // "picked" | "none" | "rejected"
    var label: String?
    let orientation: Int
    let camera: String?
    let lens: String?
    let iso: Int?
    let exposureTime: Double?
    let fNumber: Double?
    let focalLength: Double?

    var exposureSummary: String {
        var parts: [String] = []
        if let t = exposureTime { parts.append(t >= 1 ? String(format: "%.1fs", t) : "1/\(Int((1 / t).rounded()))s") }
        if let f = fNumber { parts.append(String(format: "f/%.1f", f)) }
        if let i = iso { parts.append("ISO \(i)") }
        if let fl = focalLength { parts.append(String(format: "%.0f mm", fl)) }
        return parts.joined(separator: "  ")
    }
}

struct FolderItem: Codable, Identifiable, Hashable {
    let id: Int64
    let rootPath: String
    let relPath: String
    let path: String
    let imageCount: Int64

    var displayName: String {
        relPath.isEmpty ? (rootPath as NSString).lastPathComponent : relPath
    }
}

struct ImportReport: Codable {
    let importId: Int64
    let found: Int
    let imported: Int
    let duplicates: Int
    let alreadyInCatalog: Int
    let failed: [[String]]
    let thumbnailsFailed: Int
}

struct Query: Codable {
    var folderId: Int64? = nil
    var recursive: Bool = false
    var minRating: Int = 0
    var flag: Int? = nil
    var label: String? = nil
}

enum CoreError: LocalizedError {
    case message(String)
    var errorDescription: String? {
        if case .message(let m) = self { return m }
        return nil
    }
}

private struct ErrorBox: Decodable { let error: String }

private final class ProgressBox {
    let callback: (Int, Int) -> Void
    init(_ callback: @escaping (Int, Int) -> Void) { self.callback = callback }
}

final class Core: @unchecked Sendable {
    private let handle: OpaquePointer
    var handlePointer: OpaquePointer { handle }

    init(catalogPath: String) throws {
        var err: UnsafeMutablePointer<CChar>?
        guard let h = pe_catalog_open(catalogPath, true, &err) else {
            let msg = err.map { String(cString: $0) } ?? "could not open catalog"
            if let err { pe_string_free(err) }
            throw CoreError.message(msg)
        }
        handle = h
    }

    deinit { pe_catalog_close(handle) }

    private func decode<T: Decodable>(_ p: UnsafeMutablePointer<CChar>?) throws -> T {
        guard let p else { throw CoreError.message("no response from core") }
        defer { pe_string_free(p) }
        let data = Data(bytes: p, count: strlen(p))
        if let e = try? JSONDecoder().decode(ErrorBox.self, from: data) { throw CoreError.message(e.error) }
        return try JSONDecoder().decode(T.self, from: data)
    }

    private func json<T: Encodable>(_ v: T) -> String {
        String(data: (try? JSONEncoder().encode(v)) ?? Data("null".utf8), encoding: .utf8) ?? "null"
    }

    func folders() throws -> [FolderItem] { try decode(pe_catalog_folders_json(handle)) }

    func images(_ q: Query) throws -> [ImageItem] { try decode(pe_catalog_images_json(handle, json(q))) }

    func importSources(_ paths: [String], options: [String: Any], progress: @escaping (Int, Int) -> Void) throws -> ImportReport {
        let opts = String(data: try JSONSerialization.data(withJSONObject: options), encoding: .utf8)!
        let box = Unmanaged.passRetained(ProgressBox(progress)).toOpaque()
        defer { Unmanaged<ProgressBox>.fromOpaque(box).release() }
        let result = pe_catalog_import_json(handle, json(paths), opts, { ctx, done, total in
            guard let ctx else { return }
            Unmanaged<ProgressBox>.fromOpaque(ctx).takeUnretainedValue().callback(Int(done), Int(total))
        }, box)
        return try decode(result)
    }

    @discardableResult func setRating(_ ids: [Int64], _ rating: Int) -> Bool {
        ids.withUnsafeBufferPointer { pe_catalog_set_rating(handle, $0.baseAddress, ids.count, UInt8(rating)) }
    }

    @discardableResult func setFlag(_ ids: [Int64], _ flag: Int) -> Bool {
        ids.withUnsafeBufferPointer { pe_catalog_set_flag(handle, $0.baseAddress, ids.count, Int8(flag)) }
    }

    @discardableResult func setLabel(_ ids: [Int64], _ label: String?) -> Bool {
        ids.withUnsafeBufferPointer { buf in
            if let label { return pe_catalog_set_label(handle, buf.baseAddress, ids.count, label) }
            return pe_catalog_set_label(handle, buf.baseAddress, ids.count, nil)
        }
    }

    func thumbnail(_ id: Int64) -> NSImage? {
        var len = 0
        guard let p = pe_catalog_thumbnail(handle, id, &len) else { return nil }
        defer { pe_bytes_free(p, len) }
        return NSImage(data: Data(bytes: p, count: len))
    }

    static func quickImage(path: String, orientation: Int, longEdge: Int) -> NSImage? {
        var w: UInt32 = 0, h: UInt32 = 0
        guard let p = pe_quick_image(path, UInt8(orientation), UInt32(longEdge), &w, &h) else { return nil }
        defer { pe_bytes_free(p, Int(w) * Int(h) * 4) }
        return makeImage(p, Int(w), Int(h))
    }

    static func render(path: String, settingsJSON: String? = nil, longEdge: Int) -> NSImage? {
        var w: UInt32 = 0, h: UInt32 = 0
        guard let p = pe_render(path, settingsJSON, UInt32(longEdge), &w, &h) else { return nil }
        defer { pe_bytes_free(p, Int(w) * Int(h) * 4) }
        return makeImage(p, Int(w), Int(h))
    }

    static func imageFromRGBA(_ p: UnsafeMutablePointer<UInt8>, _ w: Int, _ h: Int) -> NSImage? { makeImage(p, w, h) }

    private static func makeImage(_ p: UnsafeMutablePointer<UInt8>, _ w: Int, _ h: Int) -> NSImage? {
        let data = Data(bytes: p, count: w * h * 4) as CFData
        guard let provider = CGDataProvider(data: data),
              let cg = CGImage(width: w, height: h, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: w * 4,
                               space: CGColorSpace(name: CGColorSpace.sRGB)!,
                               bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipLast.rawValue),
                               provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent)
        else { return nil }
        return NSImage(cgImage: cg, size: NSSize(width: w, height: h))
    }
}
