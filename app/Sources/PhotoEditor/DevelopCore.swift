// Develop-model types and FFI wrappers (settings, history, presets, interactive preview).

import AppKit
import CPeFFI
import Foundation

struct WhiteBalance: Codable, Hashable {
    var mode: String  // "asShot" | "auto" | "custom"
    var temperature: Double?
    var tint: Double?
}

struct DevelopSettings: Codable, Hashable {
    var processVersion = 1
    var whiteBalance = WhiteBalance(mode: "asShot")
    var exposure = 0.0
    var contrast = 0.0
    var highlights = 0.0
    var shadows = 0.0
    var whites = 0.0
    var blacks = 0.0
    var toneCurve: [[Double]] = []
    var demosaic = "auto"
    var applyOrientation = true

    subscript(key: String) -> Double {
        get {
            switch key {
            case "exposure": return exposure
            case "contrast": return contrast
            case "highlights": return highlights
            case "shadows": return shadows
            case "whites": return whites
            case "blacks": return blacks
            default: return 0
            }
        }
        set {
            switch key {
            case "exposure": exposure = newValue
            case "contrast": contrast = newValue
            case "highlights": highlights = newValue
            case "shadows": shadows = newValue
            case "whites": whites = newValue
            case "blacks": blacks = newValue
            default: break
            }
        }
    }

    var json: String { String(data: (try? JSONEncoder().encode(self)) ?? Data("{}".utf8), encoding: .utf8) ?? "{}" }
    var jsonObject: Any { (try? JSONSerialization.jsonObject(with: JSONEncoder().encode(self))) ?? [:] }
}

struct ParamSpec: Codable, Hashable {
    let key: String
    let label: String
    let group: String
    let min: Double
    let max: Double
    let step: Double
    let `default`: Double
    let precision: Int
}

struct GroupSpec: Codable, Hashable {
    let id: String
    let label: String
    let look: Bool
}

struct DevelopSchema: Codable {
    let params: [ParamSpec]
    let temperature: ParamSpec
    let tint: ParamSpec
    let groups: [GroupSpec]

    static let shared: DevelopSchema = {
        guard let p = pe_develop_schema_json() else { fatalError("no schema") }
        defer { pe_string_free(p) }
        return try! JSONDecoder().decode(DevelopSchema.self, from: Data(bytes: p, count: strlen(p)))
    }()
}

struct HistoryStep: Codable, Hashable, Identifiable {
    let id: Int64
    let label: String
    let createdAt: Int64
}

struct SnapshotItem: Codable, Hashable, Identifiable {
    let id: Int64
    let name: String
    let createdAt: Int64
}

struct DevelopState: Codable {
    let imageId: Int64
    let settings: DevelopSettings
    let digest: String
    let edited: Bool
    let currentStep: Int64?
    let history: [HistoryStep]
    let snapshots: [SnapshotItem]
    let canUndo: Bool
    let canRedo: Bool
}

struct PresetItem: Codable, Hashable, Identifiable {
    let id: Int64
    let uuid: String
    let name: String
    let groups: [String]
    let settings: DevelopSettings
}

private struct ErrorReply: Decodable { let error: String }

private func decodeOwned<T: Decodable>(_ p: UnsafeMutablePointer<CChar>?) throws -> T {
    guard let p else { throw CoreError.message("no response from core") }
    defer { pe_string_free(p) }
    let data = Data(bytes: p, count: strlen(p))
    if let e = try? JSONDecoder().decode(ErrorReply.self, from: data) { throw CoreError.message(e.error) }
    return try JSONDecoder().decode(T.self, from: data)
}

extension Core {
    /// Renderer used for live previews: the GPU's name (e.g. "Apple M1 (Metal)") or "CPU".
    static let rendererName: String = {
        guard let p = pe_renderer_name() else { return "CPU" }
        defer { pe_string_free(p) }
        return String(cString: p)
    }()

    func developState(_ id: Int64) throws -> DevelopState {
        try decodeOwned(pe_catalog_develop_state_json(handlePointer, id))
    }

    /// Run a develop command (e.g. `["command": "undo"]`) on images; returns their new states.
    @discardableResult
    func develop(_ ids: [Int64], _ command: [String: Any]) throws -> [DevelopState] {
        let json = String(data: try JSONSerialization.data(withJSONObject: command), encoding: .utf8)!
        return try ids.withUnsafeBufferPointer { buf in
            try decodeOwned(pe_catalog_develop_json(handlePointer, buf.baseAddress, ids.count, json))
        }
    }

    func presets() throws -> [PresetItem] { try decodeOwned(pe_catalog_presets_json(handlePointer)) }

    @discardableResult
    func savePreset(name: String, groups: [String], settings: DevelopSettings) throws -> PresetItem {
        let g = String(data: try JSONEncoder().encode(groups), encoding: .utf8)!
        return try decodeOwned(pe_catalog_save_preset_json(handlePointer, name, g, settings.json))
    }

    func deletePreset(_ id: Int64) { _ = pe_catalog_delete_preset(handlePointer, id) }
}

/// A raw decoded once and reduced for fast re-rendering while sliders move.
final class PreviewImage: @unchecked Sendable {
    private let handle: OpaquePointer
    let asShotTemperature: Double?
    let asShotTint: Double?

    private struct Info: Decodable {
        let asShotTemperature: Double?
        let asShotTint: Double?
    }

    init?(path: String, longEdge: Int) {
        guard let h = pe_image_open(path, UInt32(longEdge)) else { return nil }
        handle = h
        let info: Info? = try? decodeOwned(pe_image_info_json(h))
        asShotTemperature = info?.asShotTemperature
        asShotTint = info?.asShotTint
    }

    deinit { pe_image_close(handle) }

    func render(_ settings: DevelopSettings) -> NSImage? {
        var w: UInt32 = 0, h: UInt32 = 0
        guard let p = pe_image_render(handle, settings.json, &w, &h) else { return nil }
        defer { pe_bytes_free(p, Int(w) * Int(h) * 4) }
        return Core.imageFromRGBA(p, Int(w), Int(h))
    }
}
