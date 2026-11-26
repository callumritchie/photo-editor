import CPeFFI
import AppKit
import UniformTypeIdentifiers
import SwiftUI

enum SidebarItem: Hashable {
    case all
    case folder(Int64)
    case collection(Int64)
    case keyword(Int64)
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
    @Published var searchText = ""
    @Published var flagFilter: Int?
    @Published var labelFilter: String?
    @Published var sort = "captureTime"
    @Published var descending = false
    @Published var collapseStacks = true
    @Published var collections: [CollectionItem] = []
    @Published var keywords: [KeywordItem] = []
    @Published var selectionKeywords: [KeywordItem] = []
    @Published var showKeywords = false
    @Published var collectionSheet: CollectionSheet?
    @Published var keywordRename: KeywordItem?
    @Published var confirmRemove: [Int64]?
    @Published var selection: Set<Int64> = []
    @Published var focused: Int64?
    @Published var mode: ViewMode = .grid
    @Published var thumbSize: Double = 180

    @Published var showImport = false
    @Published var importProgress: (done: Int, total: Int)?
    @Published var lastReport: ImportReport?

    @Published var showExport = false
    @Published var exportProgress: (done: Int, total: Int)?
    @Published var lastExport: ExportReport?
    @Published var exportPresets: [ExportPresetItem] = []

    /// Background renders still queued (previews and edited thumbnails).
    @Published var pendingRenders = 0
    private var watchingRenders = false

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
        if let lensDB = Bundle.main.resourceURL?.appendingPathComponent("lensfun").path,
           FileManager.default.fileExists(atPath: lensDB) {
            pe_set_lens_database(lensDB)
        }
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
        showKeywords = envs["PE_SHOW_KEYWORDS"] != nil
        // PE_EDIT_COLLECTION=<id>: open a smart collection's rule editor (UI snapshots).
        if let cid = Int64(envs["PE_EDIT_COLLECTION"] ?? ""), let c = collections.first(where: { $0.id == cid }) {
            collectionSheet = .edit(c)
        }
        exportPresets = (try? core?.exportPresets()) ?? []
        core?.onRendered { [weak self] id, thumbChanged in
            Task { @MainActor in self?.rendered(id, thumbChanged: thumbChanged) }
        }
        if envs["PE_START_EXPORT"] != nil, selection.isEmpty == false { showExport = true }
    }

    var focusedItem: ImageItem? { images.first { $0.id == focused } }

    func reload() {
        guard let core else { return }
        var q = Query(minRating: minRating)
        switch sidebar {
        case .folder(let id): q.folderId = id; q.recursive = true
        case .collection(let id): q.collectionId = id
        case .keyword(let id): q.keywordId = id
        default: break
        }
        let text = searchText.trimmingCharacters(in: .whitespaces)
        q.text = text.isEmpty ? nil : text
        q.sort = sort
        q.descending = descending
        q.collapseStacks = collapseStacks
        q.flag = flagFilter
        q.label = labelFilter
        do {
            folders = try core.folders()
            collections = (try? core.library(["command": "collections"])) ?? []
            keywords = (try? core.library(["command": "keywords"])) ?? []
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
            // Refresh the cached preview of the photo we are leaving (skipped if already current).
            if let previous = develop?.item.id { core.requestRenders([previous], urgent: false); watchRenders() }
            develop = DevelopModel(item: item, core: core) { [weak self] id in self?.editsChanged([id]) }
        }
        return develop
    }

    // MARK: background renders

    private func rendered(_ id: Int64, thumbChanged: Bool) {
        if thumbChanged { editsChanged([id]) }
        pendingRenders = core?.pendingRenders ?? 0
    }

    /// Cached loupe preview for an instant first image (not queued: the loupe renders it live).
    func cachedPreview(_ id: Int64) async -> NSImage? {
        guard let core else { return nil }
        return await Task.detached(priority: .userInitiated) { core.preview(id, queueIfStale: false)?.image }.value
    }

    /// Render the previews of the photos next to `id` ahead of time, nearest first.
    func prefetchAround(_ id: Int64) {
        guard let core, let i = images.firstIndex(where: { $0.id == id }) else { return }
        let order = [1, -1, 2, 3, -2]
        let ids = order.compactMap { images.indices.contains(i + $0) ? images[i + $0].id : nil }
        core.requestRenders(ids, urgent: true)
        watchRenders()
    }

    /// Build previews for the selection, or for every photo shown when nothing is selected.
    func buildPreviews() {
        guard let core else { return }
        let ids = selection.isEmpty ? images.map(\.id) : images.map(\.id).filter { selection.contains($0) }
        core.requestRenders(ids, urgent: false)
        watchRenders()
    }

    /// Keep `pendingRenders` current while the queue drains (jobs that are already up to date finish
    /// without a callback).
    private func watchRenders() {
        guard !watchingRenders else { return }
        watchingRenders = true
        Task { @MainActor in
            repeat {
                pendingRenders = core?.pendingRenders ?? 0
                try? await Task.sleep(for: .milliseconds(500))
            } while pendingRenders > 0
            watchingRenders = false
        }
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

    /// Strength for the next preset applied (0...2), shared by the panel and the grid menu.
    @Published var presetAmount: Double = 1.0
    /// What an import of presets could not carry over (shown once).
    @Published var presetImportNote: String?

    func updatePreset(_ id: Int64, name: String, groups: [String], settings: DevelopSettings?) {
        guard let core else { return }
        do {
            try core.updatePreset(id, name: name, groups: groups, settings: settings)
            presets = try core.presets()
        } catch { errorMessage = error.localizedDescription }
    }

    func duplicatePreset(_ id: Int64) {
        guard let core else { return }
        do {
            try core.duplicatePreset(id)
            presets = try core.presets()
        } catch { errorMessage = error.localizedDescription }
    }

    /// Apply a preset to photos (the selection from the grid) at the current amount.
    func applyPreset(_ id: Int64, to ids: [Int64]) {
        runDevelop(ids, ["command": "applyPreset", "presetId": id, "amount": presetAmount])
    }

    /// Save a preset to a `.pepreset` file chosen by the user.
    func exportPreset(_ p: PresetItem) {
        guard let core else { return }
        let panel = NSSavePanel()
        panel.nameFieldStringValue = "\(p.name).pepreset"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do { try core.exportPreset(p.id).write(to: url, atomically: true, encoding: .utf8) }
        catch { errorMessage = error.localizedDescription }
    }

    /// Import preset files (`.pepreset`, Adobe-style `.xmp`) chosen by the user.
    func importPresets() {
        guard let core else { return }
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        panel.allowedContentTypes = [.init(filenameExtension: "pepreset")!, .init(filenameExtension: "xmp")!]
        guard panel.runModal() == .OK else { return }
        var notes: [String] = []
        var failures: [String] = []
        for url in panel.urls {
            do {
                let text = try String(contentsOf: url, encoding: .utf8)
                let r = try core.importPreset(text: text, fileName: url.lastPathComponent)
                if !r.unmapped.isEmpty {
                    notes.append("\(r.preset.name): \(r.unmapped.joined(separator: ", "))")
                }
            } catch { failures.append("\(url.lastPathComponent): \(error.localizedDescription)") }
        }
        presets = (try? core.presets()) ?? presets
        var message: [String] = []
        if !notes.isEmpty { message.append("Some settings have no equivalent here and were left out:\n" + notes.joined(separator: "\n")) }
        if !failures.isEmpty { message.append("Not imported:\n" + failures.joined(separator: "\n")) }
        if !message.isEmpty { presetImportNote = message.joined(separator: "\n\n") }
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

    func runImport(sources: [URL], copyTo: URL?, template: String, skipDuplicates: Bool, developPreset: Int64? = nil) {
        guard let core else { return }
        var mode: [String: Any] = ["mode": "add"]
        if let copyTo { mode = ["mode": "copy", "dest": copyTo.path, "template": template] }
        var options: [String: Any] = ["mode": mode, "skipDuplicates": skipDuplicates, "recursive": true]
        if let developPreset { options["developPreset"] = developPreset }
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

    // MARK: export

    /// Selected photos, or the focused one when nothing is selected; in grid order.
    var exportTargets: [Int64] {
        let chosen = selection.isEmpty ? Set([focused].compactMap { $0 }) : selection
        return images.map(\.id).filter { chosen.contains($0) }
    }

    /// Last-used export settings, remembered per user (a convenience, not catalog data).
    var lastExportOptions: ExportOptions {
        get {
            guard let s = UserDefaults.standard.string(forKey: "lastExportOptions"),
                  let o = try? JSONDecoder().decode(ExportOptions.self, from: Data(s.utf8)) else { return ExportOptions() }
            return o
        }
        set { UserDefaults.standard.set(newValue.json, forKey: "lastExportOptions") }
    }

    func runExport(ids: [Int64], options: ExportOptions) {
        guard let core, !ids.isEmpty else { return }
        lastExportOptions = options
        lastExport = nil
        exportProgress = (0, ids.count)
        Task.detached(priority: .userInitiated) {
            let result = Result {
                try core.export(ids, options: options) { done, total in
                    Task { @MainActor in self.exportProgress = (done, total) }
                }
            }
            await MainActor.run {
                self.exportProgress = nil
                switch result {
                case .success(let r): self.lastExport = r
                case .failure(let e): self.errorMessage = e.localizedDescription
                }
            }
        }
    }

    func saveExportPreset(name: String, options: ExportOptions) {
        guard let core else { return }
        do {
            try core.saveExportPreset(name: name, options: options)
            exportPresets = try core.exportPresets()
        } catch { errorMessage = error.localizedDescription }
    }

    func deleteExportPreset(_ id: Int64) {
        core?.deleteExportPreset(id)
        exportPresets = (try? core?.exportPresets()) ?? []
    }

    // MARK: library organisation

    /// The photos an action applies to when invoked on `id`: the selection if `id` is in it, else `id`.
    func targets(including id: Int64) -> [Int64] {
        selection.contains(id) ? exportTargets : [id]
    }

    private func library(_ command: [String: Any]) {
        guard let core else { return }
        do { try core.libraryRun(command) } catch { errorMessage = error.localizedDescription }
        reload()
    }

    func addToCollection(_ id: Int64, ids: [Int64]) {
        library(["command": "addToCollection", "id": id, "imageIds": ids])
    }

    func removeFromCollection(_ id: Int64, ids: [Int64]) {
        library(["command": "removeFromCollection", "id": id, "imageIds": ids])
    }

    func moveCollection(_ id: Int64, to parent: Int64?) {
        var cmd: [String: Any] = ["command": "moveCollection", "id": id]
        if let parent { cmd["parentId"] = parent }
        library(cmd)
    }

    func deleteCollection(_ id: Int64) {
        if sidebar == .collection(id) { sidebar = .all }
        library(["command": "deleteCollection", "id": id])
    }

    /// Create, rename or edit a collection from its sheet. Returns false (with an error shown) on failure.
    func saveCollection(_ sheet: CollectionSheet, name: String, rules: RuleSetModel?, addSelection: Bool) -> Bool {
        guard let core else { return false }
        do {
            switch sheet {
            case .create(let kind, let parent):
                var cmd: [String: Any] = ["command": "createCollection", "name": name, "kind": kind]
                if let parent { cmd["parentId"] = parent }
                if let rules { cmd["rules"] = rules.jsonObject }
                struct Created: Decodable { let id: Int64 }
                let created: Created = try core.library(cmd)
                if kind == "collection", addSelection, !exportTargets.isEmpty {
                    try core.libraryRun(["command": "addToCollection", "id": created.id, "imageIds": exportTargets])
                }
                if kind != "set" { sidebar = .collection(created.id) }
            case .edit(let c):
                try core.libraryRun(["command": "setSmartRules", "id": c.id, "rules": (rules ?? RuleSetModel()).jsonObject])
            case .rename(let c):
                try core.libraryRun(["command": "renameCollection", "id": c.id, "name": name])
            }
            reload()
            return true
        } catch {
            errorMessage = error.localizedDescription
            return false
        }
    }

    func refreshSelectionKeywords() {
        guard let core else { return }
        selectionKeywords = (try? core.library(["command": "keywordsOf", "imageIds": exportTargets])) ?? []
    }

    /// Add comma-separated keyword paths ("Animals > Birds, Night") to the selected photos.
    func addKeywords(_ text: String) {
        let paths = text.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        guard !paths.isEmpty, !exportTargets.isEmpty else { return }
        library(["command": "addKeywords", "imageIds": exportTargets, "paths": paths])
        refreshSelectionKeywords()
    }

    func removeKeyword(_ id: Int64) {
        library(["command": "removeKeyword", "imageIds": exportTargets, "keywordId": id])
        refreshSelectionKeywords()
    }

    func renameKeyword(_ id: Int64, to name: String) {
        library(["command": "renameKeyword", "id": id, "name": name])
    }

    func deleteKeyword(_ id: Int64) {
        if sidebar == .keyword(id) { sidebar = .all }
        library(["command": "deleteKeyword", "id": id])
    }

    func createVirtualCopies(_ ids: [Int64]) {
        guard let core else { return }
        var last: Int64?
        for id in ids {
            struct Created: Decodable { let id: Int64 }
            do { last = (try core.library(["command": "createVirtualCopy", "imageId": id]) as Created).id }
            catch { errorMessage = error.localizedDescription }
        }
        reload()
        if let last { select(last) }
    }

    func stack(_ ids: [Int64]) {
        // The focused photo goes on top.
        let ordered = ids.sorted { a, b in a == focused && b != focused }
        library(["command": "stack", "imageIds": ordered])
    }

    func unstack(_ ids: [Int64]) { library(["command": "unstack", "imageIds": ids]) }

    func setStackTop(_ id: Int64) { library(["command": "setStackTop", "imageId": id]) }

    func toggleStack(_ item: ImageItem) {
        guard let s = item.stackId else { return }
        library(["command": "setStackCollapsed", "stackId": s, "collapsed": !item.stackCollapsed])
    }

    func removeFromLibrary(_ ids: [Int64]) {
        library(["command": "removeImages", "imageIds": ids])
        for id in ids { thumbCache.removeObject(forKey: NSNumber(value: id)) }
    }
}
