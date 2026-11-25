import AppKit
import SwiftUI

enum SidebarItem: Hashable {
    case all
    case folder(Int64)
}

enum ViewMode { case grid, loupe }

@MainActor
final class AppState: ObservableObject {
    @Published var core: Core?
    @Published var catalogPath: String
    @Published var errorMessage: String?

    @Published var folders: [FolderItem] = []
    @Published var images: [ImageItem] = []
    @Published var sidebar: SidebarItem? = .all
    @Published var minRating = 0
    @Published var selection: Set<Int64> = []
    @Published var focused: Int64?
    @Published var mode: ViewMode = .grid
    @Published var thumbSize: Double = 180

    @Published var showImport = false
    @Published var importProgress: (done: Int, total: Int)?
    @Published var lastReport: ImportReport?

    @Published var copiedSettings: DevelopSettings?
    @Published var presets: [PresetItem] = []
    /// Bumped per image when its edits change, so grid cells reload their thumbnail.
    @Published var thumbVersion: [Int64: Int] = [:]
    @Published private(set) var develop: DevelopModel?

    let thumbCache = NSCache<NSNumber, NSImage>()

    init() {
        let env = ProcessInfo.processInfo.environment["PE_CATALOG"]
        let pictures = FileManager.default.urls(for: .picturesDirectory, in: .userDomainMask).first!
        catalogPath = env ?? pictures.appendingPathComponent("PhotoEditor Library.pecat").path
        thumbCache.countLimit = 4000
        do {
            core = try Core(catalogPath: catalogPath)
        } catch {
            errorMessage = "Could not open catalog at \(catalogPath): \(error.localizedDescription)"
        }
        reload()
        // Development aids for smoke tests: PE_START_MODE=loupe, PE_START_INDEX=n.
        let envs = ProcessInfo.processInfo.environment
        if let i = Int(envs["PE_START_INDEX"] ?? ""), images.indices.contains(i) { select(images[i].id) }
        mode = envs["PE_START_MODE"] == "loupe" ? .loupe : .grid
    }

    var focusedItem: ImageItem? { images.first { $0.id == focused } }

    func reload() {
        guard let core else { return }
        var q = Query(minRating: minRating)
        if case .folder(let id) = sidebar { q.folderId = id; q.recursive = true }
        do {
            folders = try core.folders()
            images = try core.images(q)
            presets = (try? core.presets()) ?? []
            selection = selection.filter { id in images.contains { $0.id == id } }
            if let f = focused, !images.contains(where: { $0.id == f }) { focused = images.first?.id }
            if focused == nil { focused = images.first?.id }
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func thumbnail(_ id: Int64) async -> NSImage? {
        if let cached = thumbCache.object(forKey: NSNumber(value: id)) { return cached }
        guard let core else { return nil }
        let img = await Task.detached(priority: .userInitiated) { core.thumbnail(id) }.value
        if let img { thumbCache.setObject(img, forKey: NSNumber(value: id)) }
        return img
    }

    // MARK: develop

    /// The editing session for the focused photo (created on demand in loupe mode).
    func developSession() -> DevelopModel? {
        guard let core, let item = focusedItem else { return nil }
        if develop?.item.id != item.id {
            develop = DevelopModel(item: item, core: core) { [weak self] id in self?.editsChanged([id]) }
        }
        return develop
    }

    func editsChanged(_ ids: [Int64]) {
        for id in ids {
            thumbCache.removeObject(forKey: NSNumber(value: id))
            thumbVersion[id, default: 0] += 1
        }
    }

    private func runDevelop(_ ids: [Int64], _ command: [String: Any]) {
        guard let core, !ids.isEmpty else { return }
        do {
            try core.develop(ids, command)
            editsChanged(ids)
            if let d = develop, ids.contains(d.item.id) { develop = nil; _ = developSession() }
        } catch { errorMessage = error.localizedDescription }
    }

    /// Paste copied settings (look groups) onto the selection.
    func pasteSettings() {
        guard let s = copiedSettings else { return }
        let groups = DevelopSchema.shared.groups.filter(\.look).map(\.id)
        runDevelop(targets, ["command": "paste", "settings": s.jsonObject, "groups": groups])
    }

    /// Apply `settings` (look groups) to every selected photo except the focused one.
    func syncSettings(from settings: DevelopSettings) {
        let groups = DevelopSchema.shared.groups.filter(\.look).map(\.id)
        let ids = Array(selection.subtracting([focused ?? -1]))
        runDevelop(ids, ["command": "paste", "settings": settings.jsonObject, "groups": groups, "label": "Sync Settings"])
    }

    func savePreset(name: String, groups: [String], settings: DevelopSettings) {
        guard let core else { return }
        do {
            try core.savePreset(name: name, groups: groups, settings: settings)
            presets = try core.presets()
        } catch { errorMessage = error.localizedDescription }
    }

    func deletePreset(_ id: Int64) {
        core?.deletePreset(id)
        presets = (try? core?.presets()) ?? []
    }

    func undo() { if mode == .loupe { developSession()?.undo() } }
    func redo() { if mode == .loupe { developSession()?.redo() } }

    // MARK: selection and navigation

    func select(_ id: Int64, extend: Bool = false, toggle: Bool = false) {
        if toggle {
            if selection.contains(id) { selection.remove(id) } else { selection.insert(id) }
        } else if extend, let anchor = focused,
                  let a = images.firstIndex(where: { $0.id == anchor }),
                  let b = images.firstIndex(where: { $0.id == id }) {
            selection = Set(images[min(a, b)...max(a, b)].map(\.id))
        } else {
            selection = [id]
        }
        focused = id
    }

    func move(by delta: Int) {
        guard !images.isEmpty else { return }
        let i = images.firstIndex { $0.id == focused } ?? 0
        let j = max(0, min(images.count - 1, i + delta))
        select(images[j].id)
    }

    var targets: [Int64] {
        if selection.isEmpty, let f = focused { return [f] }
        return Array(selection)
    }

    // MARK: metadata edits

    func setRating(_ r: Int) {
        let ids = targets
        core?.setRating(ids, r)
        for i in images.indices where ids.contains(images[i].id) { images[i].rating = r }
        if minRating > 0 { reload() }
    }

    func setFlag(_ f: Int) {
        let ids = targets
        core?.setFlag(ids, f)
        let name = f == 1 ? "picked" : f == -1 ? "rejected" : "none"
        for i in images.indices where ids.contains(images[i].id) { images[i].flag = name }
    }

    func toggleLabel(_ label: String) {
        let ids = targets
        let allHave = images.filter { ids.contains($0.id) }.allSatisfy { $0.label == label }
        let new: String? = allHave ? nil : label
        core?.setLabel(ids, new)
        for i in images.indices where ids.contains(images[i].id) { images[i].label = new }
    }

    /// Shared keyboard handling for grid and loupe. Returns true if handled.
    func handleKey(_ chars: String) -> Bool {
        switch chars.lowercased() {
        case "0", "1", "2", "3", "4", "5": setRating(Int(chars)!)
        case "p": setFlag(1)
        case "x": setFlag(-1)
        case "u": setFlag(0)
        case "6": toggleLabel("red")
        case "7": toggleLabel("yellow")
        case "8": toggleLabel("green")
        case "9": toggleLabel("blue")
        default: return false
        }
        return true
    }

    // MARK: import

    func runImport(sources: [URL], copyTo: URL?, template: String, skipDuplicates: Bool) {
        guard let core else { return }
        var mode: [String: Any] = ["mode": "add"]
        if let copyTo { mode = ["mode": "copy", "dest": copyTo.path, "template": template] }
        let options: [String: Any] = ["mode": mode, "skipDuplicates": skipDuplicates, "recursive": true]
        let paths = sources.map(\.path)
        importProgress = (0, 0)
        Task.detached(priority: .userInitiated) {
            let result = Result {
                try core.importSources(paths, options: options) { done, total in
                    Task { @MainActor in self.importProgress = (done, total) }
                }
            }
            await MainActor.run {
                self.importProgress = nil
                switch result {
                case .success(let r): self.lastReport = r
                case .failure(let e): self.errorMessage = e.localizedDescription
                }
                self.reload()
            }
        }
    }
}
