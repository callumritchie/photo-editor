import SwiftUI

// MARK: - Sidebar sections

struct CollectionsSection: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        Section {
            ForEach(state.collections.filter { $0.parentId == nil }) { c in row(c) }
        } header: {
            HStack {
                Text("Collections")
                Spacer()
                Menu {
                    Button("New Collection…") { state.collectionSheet = .create(kind: "collection", parentId: nil) }
                    Button("New Smart Collection…") { state.collectionSheet = .create(kind: "smart", parentId: nil) }
                    Button("New Collection Set…") { state.collectionSheet = .create(kind: "set", parentId: nil) }
                } label: { Image(systemName: "plus") }
                    .menuStyle(.borderlessButton)
                    .menuIndicator(.hidden)
                    .fixedSize()
                    .help("New collection")
            }
        }
    }

    private func row(_ c: CollectionItem) -> AnyView {
        let label = HStack {
            Label(c.name, systemImage: c.icon)
            Spacer()
            if c.kind != "set" { Text("\(c.imageCount)").foregroundStyle(.secondary).monospacedDigit() }
        }
        .tag(SidebarItem.collection(c.id))
        .contextMenu { menu(c) }
        .dropDestination(for: String.self) { items, _ in
            let ids = items.compactMap(Int64.init)
            guard c.kind == "collection", !ids.isEmpty else { return false }
            state.addToCollection(c.id, ids: ids)
            return true
        }
        let children = state.collections.filter { $0.parentId == c.id }
        if c.kind == "set" {
            return AnyView(DisclosureGroup {
                ForEach(children) { child in row(child) }
            } label: { label })
        }
        return AnyView(label)
    }

    @ViewBuilder private func menu(_ c: CollectionItem) -> some View {
        if c.kind == "set" {
            Button("New Collection Inside…") { state.collectionSheet = .create(kind: "collection", parentId: c.id) }
            Button("New Smart Collection Inside…") { state.collectionSheet = .create(kind: "smart", parentId: c.id) }
            Button("New Set Inside…") { state.collectionSheet = .create(kind: "set", parentId: c.id) }
            Divider()
        }
        if c.kind == "smart" { Button("Edit Rules…") { state.collectionSheet = .edit(c) } }
        if c.kind == "collection" {
            Button("Add Selected Photos") { state.addToCollection(c.id, ids: state.exportTargets) }
                .disabled(state.exportTargets.isEmpty)
        }
        Button("Rename…") { state.collectionSheet = .rename(c) }
        Menu("Move To") {
            Button("Top Level") { state.moveCollection(c.id, to: nil) }
            ForEach(state.collections.filter { $0.kind == "set" && $0.id != c.id }) { s in
                Button(s.name) { state.moveCollection(c.id, to: s.id) }
            }
        }
        Divider()
        Button("Delete", role: .destructive) { state.deleteCollection(c.id) }
    }
}

struct KeywordsSection: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        Section("Keywords") {
            if state.keywords.isEmpty {
                Text("No keywords yet").font(.caption).foregroundStyle(.secondary)
            }
            ForEach(state.keywords) { k in
                HStack {
                    Label(k.name, systemImage: "tag")
                        .padding(.leading, CGFloat(k.depth) * 12)
                    Spacer()
                    Text("\(k.imageCount)").foregroundStyle(.secondary).monospacedDigit()
                }
                .help(k.path)
                .tag(SidebarItem.keyword(k.id))
                .contextMenu {
                    Button("Rename…") { state.keywordRename = k }
                    Button("Delete Keyword", role: .destructive) { state.deleteKeyword(k.id) }
                }
            }
        }
    }
}

// MARK: - Keyword panel (inspector)

struct KeywordPanel: View {
    @EnvironmentObject var state: AppState
    @State private var entry = ""

    var body: some View {
        let ids = state.exportTargets
        VStack(alignment: .leading, spacing: 10) {
            Text("Keywords").font(.headline)
            Text(ids.isEmpty ? "No photos selected" : "\(ids.count) photo\(ids.count == 1 ? "" : "s") selected")
                .font(.caption).foregroundStyle(.secondary)
            TextField("Add keywords (comma-separated, A > B for levels)", text: $entry)
                .onSubmit {
                    state.addKeywords(entry)
                    entry = ""
                }
                .disabled(ids.isEmpty)
            if !entry.isEmpty {
                let last = entry.split(separator: ",").last.map { $0.trimmingCharacters(in: .whitespaces).lowercased() } ?? ""
                let matches = state.keywords.filter { !last.isEmpty && $0.path.lowercased().contains(last) }.prefix(6)
                ForEach(Array(matches)) { k in
                    Button(k.path) {
                        var parts = entry.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }
                        parts.removeLast()
                        parts.append(k.path)
                        entry = parts.joined(separator: ", ") + ", "
                    }
                    .buttonStyle(.link)
                }
            }
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(state.selectionKeywords) { k in
                        HStack {
                            Image(systemName: "tag").foregroundStyle(.secondary)
                            Text(k.path).lineLimit(1)
                            Spacer()
                            if Int(k.imageCount) < ids.count {
                                Text("\(k.imageCount)/\(ids.count)").font(.caption).foregroundStyle(.secondary)
                            }
                            Button { state.removeKeyword(k.id) } label: { Image(systemName: "xmark.circle.fill") }
                                .buttonStyle(.plain).foregroundStyle(.secondary)
                                .help("Remove from the selected photos")
                        }
                    }
                }
            }
            Spacer()
        }
        .padding(12)
        .frame(minWidth: 240)
        .task(id: ids) { state.refreshSelectionKeywords() }
    }
}

// MARK: - Collection sheets

enum CollectionSheet: Identifiable {
    case create(kind: String, parentId: Int64?)
    case edit(CollectionItem)
    case rename(CollectionItem)

    var id: String {
        switch self {
        case .create(let k, let p): return "create-\(k)-\(p ?? 0)"
        case .edit(let c): return "edit-\(c.id)"
        case .rename(let c): return "rename-\(c.id)"
        }
    }
}

struct CollectionSheetView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.dismiss) private var dismiss
    let sheet: CollectionSheet
    @State private var name = ""
    @State private var rules = RuleSetModel(rules: [RuleField.named("rating").defaultRule()])
    @State private var addSelection = true

    private var kind: String {
        switch sheet {
        case .create(let k, _): return k
        case .edit(let c), .rename(let c): return c.kind
        }
    }
    private var editingRules: Bool {
        if case .rename = sheet { return false }
        return kind == "smart"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title).font(.headline)
            if case .edit = sheet {} else {
                TextField("Name", text: $name).frame(width: 320)
            }
            if case .create(let k, _) = sheet, k == "collection", !state.exportTargets.isEmpty {
                Toggle("Add the \(state.exportTargets.count) selected photos", isOn: $addSelection)
            }
            if editingRules { RulesEditor(rules: $rules) }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                Button("Save") {
                    if state.saveCollection(sheet, name: name, rules: editingRules ? rules : nil, addSelection: addSelection) {
                        dismiss()
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(needsName && name.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(20)
        .frame(minWidth: editingRules ? 620 : 360)
        .onAppear {
            switch sheet {
            case .edit(let c):
                name = c.name
                rules = c.rules ?? RuleSetModel()
            case .rename(let c): name = c.name
            case .create: break
            }
        }
    }

    private var needsName: Bool { if case .edit = sheet { return false }; return true }

    private var title: String {
        switch sheet {
        case .create(let k, _): return k == "smart" ? "New Smart Collection" : k == "set" ? "New Collection Set" : "New Collection"
        case .edit(let c): return "Rules for “\(c.name)”"
        case .rename: return "Rename"
        }
    }
}

struct RulesEditor: View {
    @EnvironmentObject var state: AppState
    @Binding var rules: RuleSetModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Match")
                Picker("", selection: $rules.match) {
                    Text("all").tag("all")
                    Text("any").tag("any")
                    Text("none").tag("none")
                }
                .labelsHidden().fixedSize()
                Text("of the following rules:")
            }
            ForEach($rules.rules) { $row in
                HStack {
                    Picker("", selection: Binding(get: { row.field }, set: { row = RuleField.named($0).defaultRule() })) {
                        ForEach(RuleField.all, id: \.id) { Text($0.label).tag($0.id) }
                    }
                    .labelsHidden().frame(width: 150)
                    let field = RuleField.named(row.field)
                    Picker("", selection: $row.op) {
                        ForEach(field.ops, id: \.0) { Text($0.1).tag($0.0) }
                    }
                    .labelsHidden().frame(width: 150)
                    valueEditor(field, $row)
                    Spacer(minLength: 0)
                    Button { rules.rules.removeAll { $0.id == row.id } } label: { Image(systemName: "minus.circle") }
                        .buttonStyle(.plain)
                }
            }
            Button { rules.rules.append(RuleField.named("rating").defaultRule()) } label: { Label("Add Rule", systemImage: "plus.circle") }
                .buttonStyle(.plain)
        }
    }

    @ViewBuilder private func valueEditor(_ field: RuleField, _ row: Binding<RuleRow>) -> some View {
        let op = row.wrappedValue.op
        if op == "empty" || op == "notEmpty" {
            EmptyView()
        } else {
            switch field.kind {
            case .number, .date:
                let isDate: Bool = { if case .date = field.kind { return op != "inLast" }; return false }()
                textBinding(row, \.value, number: !isDate, placeholder: isDate ? "YYYY-MM-DD" : "value")
                if op == "between" {
                    Text("and")
                    textBinding(row, \.value2, number: !isDate, placeholder: isDate ? "YYYY-MM-DD" : "value")
                }
            case .text:
                textBinding(row, \.value, number: false, placeholder: "text")
            case .choice(let options):
                Picker("", selection: Binding(get: { row.wrappedValue.value.stringValue ?? "" },
                                              set: { row.wrappedValue.value = .string($0) })) {
                    ForEach(options, id: \.0) { Text($0.1).tag($0.0) }
                }
                .labelsHidden().frame(width: 140)
            case .bool:
                Picker("", selection: Binding(get: { row.wrappedValue.value != .bool(false) },
                                              set: { row.wrappedValue.value = .bool($0) })) {
                    Text("yes").tag(true)
                    Text("no").tag(false)
                }
                .labelsHidden().frame(width: 80)
            case .collection:
                Picker("", selection: Binding(get: { row.wrappedValue.value.double.map { Int64($0) } ?? -1 },
                                              set: { row.wrappedValue.value = .number(Double($0)) })) {
                    Text("Choose…").tag(Int64(-1))
                    ForEach(state.collections.filter { $0.kind != "set" }) { Text($0.name).tag($0.id) }
                }
                .labelsHidden().frame(width: 160)
            }
        }
    }

    private func textBinding(_ row: Binding<RuleRow>, _ key: WritableKeyPath<RuleRow, JSONValue>, number: Bool,
                             placeholder: String) -> some View {
        TextField(placeholder, text: Binding(
            get: {
                switch row.wrappedValue[keyPath: key] {
                case .number(let n): return n == n.rounded() ? String(Int(n)) : String(n)
                case .string(let s): return s
                default: return ""
                }
            },
            set: { s in
                if number, let n = Double(s) { row.wrappedValue[keyPath: key] = .number(n) }
                else { row.wrappedValue[keyPath: key] = .string(s) }
            }))
            .frame(width: 110)
    }
}

// MARK: - Grid context menu and badges

struct PhotoContextMenu: View {
    @EnvironmentObject var state: AppState
    let item: ImageItem

    var body: some View {
        let ids = state.targets(including: item.id)
        let statics = state.collections.filter { $0.kind == "collection" }
        Menu("Add to Collection") {
            if statics.isEmpty { Text("No collections") }
            ForEach(statics) { c in Button(c.name) { state.addToCollection(c.id, ids: ids) } }
            Divider()
            Button("New Collection…") { state.collectionSheet = .create(kind: "collection", parentId: nil) }
        }
        if case .collection(let cid) = state.sidebar, state.collections.first(where: { $0.id == cid })?.kind == "collection" {
            Button("Remove from This Collection") { state.removeFromCollection(cid, ids: ids) }
        }
        Button("Keywords…") { state.showKeywords = true }
        Menu("Apply Preset") {
            if state.presets.isEmpty { Text("No presets") }
            ForEach(state.presets) { p in Button(p.name) { state.applyPreset(p.id, to: ids) } }
        }
        Divider()
        Button("Create Virtual Copy") { state.createVirtualCopies(ids) }
        Divider()
        if ids.count > 1 { Button("Group into Stack") { state.stack(ids) } }
        if item.stackId != nil {
            Button(item.stackCollapsed ? "Expand Stack" : "Collapse Stack") { state.toggleStack(item) }
            if item.stackPosition != 0 { Button("Move to Top of Stack") { state.setStackTop(item.id) } }
            Button("Remove from Stack") { state.unstack(ids) }
        }
        Divider()
        Button("Remove from Library…", role: .destructive) { state.confirmRemove = ids }
    }
}

struct ThumbBadges: View {
    @EnvironmentObject var state: AppState
    let item: ImageItem

    var body: some View {
        HStack(spacing: 4) {
            if item.masterId != nil {
                Image(systemName: "doc.on.doc").help(item.copyName ?? "Virtual copy")
            }
            if item.stackId != nil && item.stackPosition == 0 {
                Button { state.toggleStack(item) } label: {
                    HStack(spacing: 2) {
                        Image(systemName: "square.stack.3d.up")
                        Text("\(item.stackSize)").monospacedDigit()
                    }
                }
                .buttonStyle(.plain)
                .help(item.stackCollapsed ? "Expand stack" : "Collapse stack")
            } else if item.stackId != nil {
                Text("\(item.stackPosition.map { $0 + 1 } ?? 0)/\(item.stackSize)").monospacedDigit()
            }
        }
        .font(.caption2)
        .padding(.horizontal, 5).padding(.vertical, 2)
        .background(.black.opacity(item.masterId != nil || item.stackId != nil ? 0.55 : 0), in: Capsule())
        .foregroundStyle(.white)
        .padding(6)
    }
}
