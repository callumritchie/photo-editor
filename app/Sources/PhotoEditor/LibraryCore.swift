// Library organisation: collections, keywords, rules, and the core calls behind them.

import CPeFFI
import Foundation

struct CollectionItem: Codable, Identifiable, Hashable {
    let id: Int64
    let uuid: String
    let parentId: Int64?
    let kind: String  // "set" | "collection" | "smart"
    let name: String
    let rules: RuleSetModel?
    let imageCount: Int64

    var icon: String {
        switch kind {
        case "set": return "folder"
        case "smart": return "gearshape"
        default: return "rectangle.stack"
        }
    }
}

struct KeywordItem: Codable, Identifiable, Hashable {
    let id: Int64
    let parentId: Int64?
    let name: String
    let path: String
    let imageCount: Int64

    var depth: Int { path.components(separatedBy: " > ").count - 1 }
}

/// A flat rule set as edited in the smart collection editor (nested groups made elsewhere are kept
/// as they are by the core but shown here only at the top level).
struct RuleSetModel: Codable, Hashable {
    var match: String = "all"
    var rules: [RuleRow] = []

    init(match: String = "all", rules: [RuleRow] = []) {
        self.match = match
        self.rules = rules
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        match = (try? c.decode(String.self, forKey: .match)) ?? "all"
        // Skip nested groups (they have no "field").
        let raw = (try? c.decode([JSONValue].self, forKey: .rules)) ?? []
        rules = raw.compactMap { v in
            guard let data = try? JSONEncoder().encode(v) else { return nil }
            return try? JSONDecoder().decode(RuleRow.self, from: data)
        }
    }
}

struct RuleRow: Codable, Hashable, Identifiable {
    var id = UUID()
    var field: String
    var op: String
    var value: JSONValue = .null
    var value2: JSONValue = .null

    enum CodingKeys: String, CodingKey { case field, op, value, value2 }

    init(field: String, op: String, value: JSONValue = .null, value2: JSONValue = .null) {
        self.field = field
        self.op = op
        self.value = value
        self.value2 = value2
    }
}

/// How each rule field is edited.
enum FieldKind { case number, text, choice([(String, String)]), bool, date, collection }

struct RuleField {
    let id: String
    let label: String
    let kind: FieldKind

    static let all: [RuleField] = [
        RuleField(id: "rating", label: "Rating", kind: .number),
        RuleField(id: "flag", label: "Flag", kind: .choice([("picked", "Picked"), ("rejected", "Rejected"), ("unflagged", "Unflagged")])),
        RuleField(id: "label", label: "Colour label", kind: .choice([("red", "Red"), ("yellow", "Yellow"), ("green", "Green"),
                                                                       ("blue", "Blue"), ("purple", "Purple"), ("none", "None")])),
        RuleField(id: "keyword", label: "Keyword", kind: .text),
        RuleField(id: "text", label: "Any text", kind: .text),
        RuleField(id: "fileName", label: "File name", kind: .text),
        RuleField(id: "fileType", label: "File type", kind: .choice([("raw", "Raw"), ("jpeg", "JPEG")])),
        RuleField(id: "folder", label: "Folder", kind: .text),
        RuleField(id: "camera", label: "Camera", kind: .text),
        RuleField(id: "lens", label: "Lens", kind: .text),
        RuleField(id: "iso", label: "ISO", kind: .number),
        RuleField(id: "focalLength", label: "Focal length (mm)", kind: .number),
        RuleField(id: "aperture", label: "Aperture (f/)", kind: .number),
        RuleField(id: "shutterSpeed", label: "Shutter speed (s)", kind: .number),
        RuleField(id: "captureDate", label: "Capture date", kind: .date),
        RuleField(id: "edited", label: "Edited", kind: .bool),
        RuleField(id: "virtualCopy", label: "Virtual copy", kind: .bool),
        RuleField(id: "collection", label: "Collection", kind: .collection),
    ]

    static func named(_ id: String) -> RuleField { all.first { $0.id == id } ?? all[0] }

    /// Operators offered for this field, as (id, label).
    var ops: [(String, String)] {
        switch kind {
        case .number:
            return [("is", "is"), ("isNot", "is not"), ("ge", "is at least"), ("le", "is at most"),
                    ("gt", "is greater than"), ("lt", "is less than"), ("between", "is between"),
                    ("empty", "is missing"), ("notEmpty", "is present")]
        case .text:
            if id == "text" { return [("contains", "contains"), ("notContains", "does not contain")] }
            var o = [("contains", "contains"), ("notContains", "does not contain"), ("is", "is"), ("isNot", "is not"),
                     ("startsWith", "starts with"), ("endsWith", "ends with"), ("empty", "is empty"), ("notEmpty", "is not empty")]
            if id == "keyword" { o[2] = ("is", "is (or below)"); o[3] = ("isNot", "is not (or below)") }
            return o
        case .choice: return [("is", "is"), ("isNot", "is not")]
        case .bool: return [("is", "is")]
        case .date: return [("is", "is"), ("lt", "is before"), ("gt", "is after"), ("between", "is between"), ("inLast", "is in the last (days)")]
        case .collection: return [("is", "is in"), ("isNot", "is not in")]
        }
    }

    /// A sensible starting rule for this field.
    func defaultRule() -> RuleRow {
        switch kind {
        case .number: return RuleRow(field: id, op: "ge", value: .number(id == "rating" ? 3 : 0))
        case .text: return RuleRow(field: id, op: "contains", value: .string(""))
        case .choice(let c): return RuleRow(field: id, op: "is", value: .string(c.first?.0 ?? ""))
        case .bool: return RuleRow(field: id, op: "is", value: .bool(true))
        case .date: return RuleRow(field: id, op: "inLast", value: .number(30))
        case .collection: return RuleRow(field: id, op: "is", value: .null)
        }
    }
}

private struct LibraryError: Decodable { let error: String }

extension Core {
    /// Run a library command (see pe_catalog::LibraryCommand) and decode its result.
    func library<T: Decodable>(_ command: [String: Any], as: T.Type = T.self) throws -> T {
        let json = String(data: try JSONSerialization.data(withJSONObject: command), encoding: .utf8)!
        guard let p = pe_catalog_library_json(handlePointer, json) else { throw CoreError.message("no response from core") }
        defer { pe_string_free(p) }
        let data = Data(bytes: p, count: strlen(p))
        if let e = try? JSONDecoder().decode(LibraryError.self, from: data) { throw CoreError.message(e.error) }
        return try JSONDecoder().decode(T.self, from: data)
    }

    /// Run a library command whose result is not needed.
    func libraryRun(_ command: [String: Any]) throws {
        let _: JSONValue = try library(command)
    }
}

extension RuleSetModel {
    /// JSON object for library commands.
    var jsonObject: Any { (try? JSONSerialization.jsonObject(with: JSONEncoder().encode(self))) ?? [:] }
}
