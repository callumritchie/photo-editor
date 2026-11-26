// Develop-model types and FFI wrappers (settings, history, presets, interactive preview).

import AppKit
import CPeFFI
import Foundation

struct WhiteBalance: Codable, Hashable {
    var mode: String  // "asShot" | "auto" | "custom"
    var temperature: Double?
    var tint: Double?
}

/// Any JSON value, so settings can be edited by path without Swift knowing every field.
enum JSONValue: Codable, Hashable {
    case null, bool(Bool), number(Double), string(String), array([JSONValue]), object([String: JSONValue])

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let b = try? c.decode(Bool.self) { self = .bool(b) }
        else if let n = try? c.decode(Double.self) { self = .number(n) }
        else if let s = try? c.decode(String.self) { self = .string(s) }
        else if let a = try? c.decode([JSONValue].self) { self = .array(a) }
        else { self = .object(try c.decode([String: JSONValue].self)) }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }

    /// Value at a dotted path such as `hsl.hue.0`.
    subscript(path: ArraySlice<Substring>) -> JSONValue? {
        get {
            guard let head = path.first else { return self }
            switch self {
            case .object(let o): return o[String(head)]?[path.dropFirst()]
            case .array(let a): guard let i = Int(head), a.indices.contains(i) else { return nil }
                return a[i][path.dropFirst()]
            default: return nil
            }
        }
        set {
            guard let head = path.first else { self = newValue ?? .null; return }
            switch self {
            case .object(var o):
                var child = o[String(head)] ?? .object([:])
                child[path.dropFirst()] = newValue
                o[String(head)] = child
                self = .object(o)
            case .array(var a):
                guard let i = Int(head), a.indices.contains(i) else { return }
                a[i][path.dropFirst()] = newValue
                self = .array(a)
            default:
                var fresh = JSONValue.object([:])
                fresh[path] = newValue
                self = fresh
            }
        }
    }

    var double: Double? { if case .number(let n) = self { return n }; return nil }
    var stringValue: String? { if case .string(let s) = self { return s }; return nil }
}

/// Develop settings as the core's JSON. Sliders are read and written by dotted path (from the schema);
/// missing values mean "default", which the core fills in.
struct DevelopSettings: Codable, Hashable {
    var value: JSONValue = .object([:])

    init() {}
    init(from decoder: Decoder) throws { value = try JSONValue(from: decoder) }
    func encode(to encoder: Encoder) throws { try value.encode(to: encoder) }

    private static func split(_ path: String) -> ArraySlice<Substring> { path.split(separator: ".")[...] }

    func number(_ path: String) -> Double? { value[Self.split(path)]?.double }
    mutating func setNumber(_ path: String, _ v: Double) { value[Self.split(path)] = .number(v) }
    func string(_ path: String) -> String? { value[Self.split(path)]?.stringValue }
    mutating func setString(_ path: String, _ v: String) { value[Self.split(path)] = .string(v) }

    var whiteBalance: WhiteBalance {
        get {
            WhiteBalance(mode: string("whiteBalance.mode") ?? "asShot",
                         temperature: number("whiteBalance.temperature"), tint: number("whiteBalance.tint"))
        }
        set {
            var o: [String: JSONValue] = ["mode": .string(newValue.mode)]
            if let t = newValue.temperature { o["temperature"] = .number(t) }
            if let t = newValue.tint { o["tint"] = .number(t) }
            value[["whiteBalance"][...]] = .object(o)
        }
    }

    var treatment: String {
        get { string("treatment") ?? "color" }
        set { setString("treatment", newValue) }
    }

    var json: String { String(data: (try? JSONEncoder().encode(self)) ?? Data("{}".utf8), encoding: .utf8) ?? "{}" }
    var jsonObject: Any { (try? JSONSerialization.jsonObject(with: JSONEncoder().encode(self))) ?? [:] }
}

struct ParamSpec: Codable, Hashable {
    let key: String
    let label: String
    let group: String
    var section: String = ""
    var subsection: String? = nil
    var when: String? = nil
    var hue: Bool = false
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

struct ChoiceSpec: Codable, Hashable {
    let key: String
    let label: String
    let section: String
    let options: [[String]]
}

struct DevelopSchema: Codable {
    let params: [ParamSpec]
    let maskParams: [ParamSpec]
    let choices: [ChoiceSpec]
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
    var handlePointer: OpaquePointer { handle }
    let asShotTemperature: Double?
    let asShotTint: Double?
    /// Preview size in sensor orientation.
    let width: Int
    let height: Int

    private struct Info: Decodable {
        let width: Int
        let height: Int
        let asShotTemperature: Double?
        let asShotTint: Double?
    }

    init?(path: String, longEdge: Int) {
        guard let h = pe_image_open(path, UInt32(longEdge)) else { return nil }
        handle = h
        let info: Info? = try? decodeOwned(pe_image_info_json(h))
        asShotTemperature = info?.asShotTemperature
        asShotTint = info?.asShotTint
        width = info?.width ?? 1
        height = info?.height ?? 1
    }

    deinit { pe_image_close(handle) }

    func render(_ settings: DevelopSettings) -> NSImage? {
        var w: UInt32 = 0, h: UInt32 = 0
        guard let p = pe_image_render(handle, settings.json, &w, &h) else { return nil }
        defer { pe_bytes_free(p, Int(w) * Int(h) * 4) }
        return Core.imageFromRGBA(p, Int(w), Int(h))
    }
}
