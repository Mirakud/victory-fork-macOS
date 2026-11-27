// swiftc -O victory_perf_hud.swift -o victory-perf-hud
// victory-perf-hud /absolute/path/to/telemetry.json
// victory-perf-hud --self-test /absolute/path/to/snapshot.png

import AppKit
import Carbon.HIToolbox
import CoreGraphics
import Darwin
import IOKit.hid

private struct Sample: Decodable {
    var game_pid: pid_t?
    var fps: Double?
    var ram_mib: Double?
    var cpu_percent: Double?
    var frame_ms: Double?
    var gpu_ms: Double?
    var copy_ms: Double?
    var ended: Bool?
}

private let french = Locale(identifier: "fr_FR")

private func fpsText(_ value: Double?) -> String {
    guard let value, value.isFinite, value >= 0 else { return "—" }
    if value == 0 { return "0" }
    if value < 0.001 { return "< 0,001" }
    let decimals = value < 0.1 ? 3 : (value < 1 ? 2 : 1)
    return String(format: "%.*f", locale: french, decimals, value)
}

private func ramText(_ value: Double?) -> String {
    guard let value, value.isFinite, value >= 0 else { return "—" }
    return String(format: "%.2f Gio", locale: french, value / 1024)
}

private func milliseconds(_ value: Double?) -> String {
    guard let value, value.isFinite, value >= 0 else { return "—" }
    return String(format: "%.*f ms", locale: french, value < 100 ? 1 : 0, value)
}

private struct Readings: Equatable {
    let fps, cpu, ram, frame, gpu, copies: String

    init(_ sample: Sample) {
        fps = fpsText(sample.fps)
        ram = ramText(sample.ram_mib)
        if let load = sample.cpu_percent, load.isFinite, load >= 0 {
            cpu = String(format: "%.*f %%", locale: french, load < 100 ? 1 : 0, load)
        } else {
            cpu = "—"
        }
        frame = milliseconds(sample.frame_ms)
        gpu = milliseconds(sample.gpu_ms)
        copies = milliseconds(sample.copy_ms)
    }
}

@MainActor
private final class MetricsView: NSView {
    static let size = NSSize(width: 300, height: 118)
    private(set) var values = Readings(Sample())

    override var isFlipped: Bool { true }

    func update(_ sample: Sample) {
        let next = Readings(sample)
        if next != values {
            values = next
            needsDisplay = true
        }
        setAccessibilityElement(true)
        setAccessibilityRole(.group)
        setAccessibilityLabel("mirakud, \(next.fps) FPS, CPU Victory \(next.cpu), RAM Victory \(next.ram), temps image \(next.frame), attente GPU par image \(next.gpu), copies par image \(next.copies)")
    }

    override func draw(_ dirtyRect: NSRect) {
        let background = NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5),
                                      xRadius: 10, yRadius: 10)
        NSColor(srgbRed: 0.055, green: 0.065, blue: 0.078, alpha: 0.90).setFill()
        background.fill()
        NSColor(white: 1, alpha: 0.23).setStroke()
        background.lineWidth = 1
        background.stroke()
        NSColor(white: 1, alpha: 0.20).setFill()
        NSBezierPath(rect: NSRect(x: 12, y: 71, width: bounds.width - 24, height: 0.5)).fill()

        func text(_ value: String, x: CGFloat, y: CGFloat, font: NSFont, color: NSColor) {
            (value as NSString).draw(at: NSPoint(x: x, y: y), withAttributes: [
                .font: font, .foregroundColor: color,
            ])
        }
        func metric(_ label: String, _ value: String, x: CGFloat, y: CGFloat) {
            text(label, x: x, y: y, font: .systemFont(ofSize: 10, weight: .medium), color: NSColor(white: 0.83, alpha: 1))
            text(value, x: x, y: y + 13, font: .monospacedDigitSystemFont(ofSize: 16, weight: .semibold), color: .white)
        }
        text("mirakud", x: 12, y: 4, font: .systemFont(ofSize: 19, weight: .bold),
             color: NSColor(srgbRed: 0.69, green: 0.86, blue: 0.79, alpha: 1))
        metric("FPS", values.fps, x: 12, y: 32)
        metric("CPU Victory", values.cpu, x: 108, y: 32)
        metric("RAM Victory", values.ram, x: 204, y: 32)
        metric("Temps image", values.frame, x: 12, y: 78)
        metric("Attente GPU", values.gpu, x: 108, y: 78)
        metric("Copies / image", values.copies, x: 204, y: 78)
    }
}

@MainActor
private final class GamePanel: NSPanel {
    let metrics = MetricsView(frame: NSRect(origin: .zero, size: MetricsView.size))
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }

    init() {
        super.init(contentRect: NSRect(origin: .zero, size: MetricsView.size),
                   styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: false)
        isOpaque = false
        backgroundColor = .clear
        hasShadow = false
        level = .floating
        isFloatingPanel = true
        hidesOnDeactivate = false
        ignoresMouseEvents = true
        isExcludedFromWindowsMenu = true
        collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .ignoresCycle]
        contentView = metrics
    }
}

private struct PipelineInventory: Decodable {
    struct Pipeline: Decodable {
        let key, pair: String
        let drawn, skipped: UInt64
        let frame_drawn, frame_skipped: UInt64?
    }
    let version: Int
    let revision: UInt64
    let mode, kind, target, error: String
    let pipelines: [Pipeline]
}

private struct ShaderFilter: Equatable {
    enum Kind: String {
        case key, pair

        func accepts(_ id: String) -> Bool {
            let parts = id.split(separator: "/", omittingEmptySubsequences: false)
            let width = self == .key ? 32 : 16
            return parts.count == (self == .key ? 1 : 2) && parts.allSatisfy {
                $0.utf8.count == width && $0.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
            }
        }
    }
    var mode = "all"
    var kind = Kind.key
    var targets: [String] = []

    func contains(_ choice: String, in family: Kind) -> Bool {
        kind == family && targets.contains(choice)
    }

    func toggling(_ choice: String, kind family: Kind, solo: Bool) -> ShaderFilter? {
        guard family.accepts(choice) else { return nil }
        let nextMode = solo ? "solo" : "hide"
        var result = mode == nextMode && kind == family ? targets : []
        if let index = result.firstIndex(of: choice) {
            result.remove(at: index)
        } else {
            guard result.count < 32 else { return nil }
            result.append(choice)
        }
        return result.isEmpty ? ShaderFilter() : ShaderFilter(mode: nextMode, kind: family, targets: result)
    }

    func command(revision: UInt64) -> String {
        mode == "all" ? "\(revision) all\n" : "\(revision) \(mode) \(kind.rawValue) \(targets.joined(separator: ","))\n"
    }
}

private struct ShaderPairName {
    let title: String
    let character: Bool
    let details: String

    init(pair: String, names: [String: String]) {
        let ids = pair.components(separatedBy: "/")
        let stages = ids.map { names[$0] }
        let base = stages.map { name in
            (name ?? "").replacingOccurrences(of: "_(?:[0-9a-fA-F]{8}_)?Wrapped$", with: "", options: .regularExpression)
        }
        let words = base.map { $0.lowercased() }
        let material = words.last ?? ""
        let joined = words.joined(separator: " ")
        let ped = joined.contains("pedtransform") || joined.contains("deferredped") || joined.contains("_ped")
        var subject: String?
        // Hiding this pair in the courtyard removes the jaw and neck from depth.
        let faceSkin = pair == "f841ad74ec6e7367/6695d7f5e3f6d351"
        if faceSkin {
            subject = "Visage et cou"
        } else if joined.contains("eyelash") || joined.contains("eyebrow") || joined.contains("eyeball") || joined.contains("_eye") {
            subject = "Yeux"
        } else if joined.contains("hair") {
            subject = "Cheveux"
        } else if joined.contains("facial") || joined.contains("pedface") || joined.contains("pedhead") || joined.contains("headblend") {
            subject = "Visage"
        } else if material.contains("material_skin") || material.contains("skinshader") {
            subject = "Peau"
        } else if ped {
            subject = "Personnage"
        }
        character = subject != nil
        let pass: String?
        if faceSkin { pass = "peau" }
        else if material.contains("reflection") { pass = "reflet" }
        else if material.contains("shadow") { pass = "ombres" }
        else if material.contains("decal") { pass = "décalcomanies" }
        else if material.contains("alphaprepass") { pass = "pré-passe alpha" }
        else if material.contains("alphaclip") { pass = "découpe alpha" }
        else if material.contains("deferredpedtextured") { pass = "matériau texturé" }
        else { pass = nil }
        let known = base.reversed().first(where: { !$0.isEmpty }) ?? ""
        let short = known.components(separatedBy: ":").last?.replacingOccurrences(of: "^(?:[A-Z0-9]+_)*(?:PS|VS|MS|GS)_", with: "", options: .regularExpression) ?? known
        if let subject {
            title = "\(subject) · \(pass ?? (short.isEmpty ? "passe inconnue" : short))"
        } else {
            title = short.isEmpty ? "Paire sans nom" : short
        }
        if ids.count == 2 {
            let stageDetails = zip(["Sommets", "Pixels"], zip(ids, stages)).map { stage, entry in
                "\(stage) · \(entry.1 ?? "nom inconnu") [\(entry.0)]"
            }.joined(separator: "\n")
            details = faceSkin ? "Tête · menton, mâchoire et avant du cou\n" + stageDetails : stageDetails
        } else {
            details = "Paire non renseignée"
        }
    }
}

private struct ShaderChoice {
    let id: String
    var uses: UInt64 = 0
    var frameUses: UInt64?
    var pairs: Set<String> = []
    var title = ""
    var details = ""
    var character = false

    var shortID: String {
        id.split(separator: "/").map { String($0.prefix(4)) }.joined(separator: "/")
    }
    var caption: String { "\(title) · \(shortID)" }
}

@MainActor
private final class ShaderControls: NSPanel, NSSearchFieldDelegate {
    static let width: CGFloat = 360
    private let directory: URL
    private let disclosure = NSButton()
    private let family = NSSegmentedControl(labels: ["Paires de shaders", "Pipelines"], trackingMode: .selectOne, target: nil, action: nil)
    private let scope = NSTextField(labelWithString: "")
    private let search = NSSearchField()
    private let characters = NSButton(checkboxWithTitle: "Personnage", target: nil, action: nil)
    private let recent = NSButton(checkboxWithTitle: "Dernière image", target: nil, action: nil)
    private let counts = NSTextField(labelWithString: "")
    private let selector = NSButton()
    private let toggle = NSButton()
    private let solo = NSButton()
    private let reset = NSButton()
    private let status = NSTextField(labelWithString: "")
    private var inventory: PipelineInventory?
    private var inventoryDate: Date?
    private var requested: (revision: UInt64, filter: ShaderFilter)?
    private var revision: UInt64 = 0
    private var issue: String?
    private var names: [String: String] = [:]
    private var namesDate: Date?
    private var pairNames: [String: ShaderPairName] = [:]
    private var choices: [ShaderChoice] = []
    private var visibleChoices: [ShaderChoice] = []
    private var selection: String?
    private var expanded = false
    var didResize: (() -> Void)?
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { false }
    private var kind: ShaderFilter.Kind { family.selectedSegment == 0 ? .pair : .key }

    init(directory: URL) {
        self.directory = directory
        super.init(contentRect: NSRect(x: 0, y: 0, width: Self.width, height: 145),
                   styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: false)
        isOpaque = false
        backgroundColor = .clear
        isFloatingPanel = true
        becomesKeyOnlyIfNeeded = true
        hidesOnDeactivate = false
        level = .floating
        isExcludedFromWindowsMenu = true
        collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .ignoresCycle]
        appearance = NSAppearance(named: .darkAqua)
        let view = NSView(frame: NSRect(origin: .zero, size: frame.size))
        view.wantsLayer = true
        view.layer?.backgroundColor = NSColor(srgbRed: 0.055, green: 0.065, blue: 0.078, alpha: 0.96).cgColor
        view.layer?.cornerRadius = 10
        contentView = view
        for button in [disclosure, selector, toggle, solo, reset] {
            button.bezelStyle = .rounded
            button.controlSize = .small
            button.font = .systemFont(ofSize: 12)
            button.target = self
            view.addSubview(button)
        }
        disclosure.isBordered = false
        disclosure.alignment = .left
        disclosure.font = .systemFont(ofSize: 13, weight: .semibold)
        disclosure.action = #selector(fold)
        family.selectedSegment = 0
        family.controlSize = .small
        family.target = self
        family.action = #selector(switchFamily)
        view.addSubview(family)
        search.placeholderString = "Rechercher un nom ou un identifiant"
        search.delegate = self
        search.setAccessibilityLabel("Rechercher dans les shaders observés")
        view.addSubview(search)
        for box in [characters, recent] {
            box.controlSize = .small
            box.target = self
            box.action = #selector(filtersChanged)
            view.addSubview(box)
        }
        characters.toolTip = "Noms du catalogue associés aux personnages, à la peau, aux yeux ou aux cheveux."
        recent.state = .on
        recent.toolTip = "Affiche les dessins rendus ou masqués dans la dernière image. Décocher pour explorer les passes des images précédentes."
        selector.action = #selector(choose)
        selector.setAccessibilityLabel("Sélectionner le pipeline ou la paire de shaders")
        toggle.action = #selector(toggleChoice)
        solo.action = #selector(isolateChoice)
        reset.title = "Tout rétablir"
        reset.action = #selector(restoreAll)
        for field in [status, scope, counts] {
            field.font = .systemFont(ofSize: 11)
            field.textColor = .secondaryLabelColor
            field.lineBreakMode = .byTruncatingTail
            view.addSubview(field)
        }
        layoutControls()
        renderControls()
    }

    private func layoutControls() {
        let height: CGFloat = expanded ? 238 : 29
        setContentSize(NSSize(width: expanded ? Self.width : MetricsView.size.width, height: height))
        func place(_ item: NSView, _ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat) {
            item.frame = NSRect(x: x, y: height - y - h, width: w, height: h)
        }
        place(disclosure, 8, 2, 282, 25)
        place(family, 10, 29, 340, 23)
        place(scope, 12, 53, 338, 17)
        place(search, 11, 74, 338, 23)
        place(characters, 11, 100, 136, 19)
        place(recent, 158, 100, 168, 19)
        place(selector, 9, 123, 342, 25)
        place(counts, 12, 151, 336, 19)
        place(toggle, 9, 177, 98, 25)
        place(solo, 109, 177, 98, 25)
        place(reset, 209, 177, 142, 25)
        place(status, 12, 208, 336, 20)
        for item: NSView in [family, scope, search, characters, recent, selector, counts, toggle, solo, reset, status] { item.isHidden = !expanded }
        disclosure.title = (expanded ? "▾" : "▸") + "  Shaders A/B"
        disclosure.setAccessibilityLabel(expanded ? "Replier les contrôles shaders" : "Déplier les contrôles shaders")
    }

    @objc private func fold() {
        expanded.toggle()
        if expanded {
            rebuildChoices()
        } else {
            resignKey()
        }
        layoutControls()
        renderControls()
        didResize?()
    }

    func refreshInventory() {
        let catalog = directory.appendingPathComponent("shader-names.json")
        let catalogDate = try? catalog.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate
        var catalogChanged = false
        if let catalogDate, catalogDate != namesDate, let data = try? Data(contentsOf: catalog),
           let next = try? JSONDecoder().decode([String: String].self, from: data) {
            names = next
            namesDate = catalogDate
            pairNames.removeAll(keepingCapacity: true)
            catalogChanged = true
        }
        let url = directory.appendingPathComponent("shader-inventory.json")
        guard let date = try? url.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate,
              date != inventoryDate else {
            if catalogChanged && expanded { rebuildChoices(); renderControls() }
            return
        }
        guard let data = try? Data(contentsOf: url),
              let next = try? JSONDecoder().decode(PipelineInventory.self, from: data),
              next.version == 1 else { return }
        inventoryDate = date
        inventory = next
        revision = max(revision, next.revision)
        if let pending = requested, next.revision >= pending.revision || !next.error.isEmpty {
            requested = nil
        }
        issue = nil
        if expanded { rebuildChoices() }
        renderControls()
    }

    private func rebuildChoices() {
        var groups: [String: ShaderChoice] = [:]
        for item in inventory?.pipelines ?? [] {
            let id = kind == .key ? item.key : item.pair
            guard kind.accepts(id) else { continue }
            var row = groups[id] ?? ShaderChoice(id: id)
            row.uses += item.drawn + item.skipped
            if let drawn = item.frame_drawn, let skipped = item.frame_skipped {
                row.frameUses = (row.frameUses ?? 0) + drawn + skipped
            }
            row.pairs.insert(item.pair)
            groups[id] = row
        }
        choices = groups.values.map { item in
            var row = item
            let descriptions = item.pairs.sorted().map { pair -> ShaderPairName in
                if let known = pairNames[pair] { return known }
                let name = ShaderPairName(pair: pair, names: names)
                pairNames[pair] = name
                return name
            }
            let titles = Set(descriptions.map(\.title))
            row.title = titles.count == 1 ? titles.first! : "\(descriptions.count) paires de shaders"
            if kind == .key && row.title == "Paire sans nom" { row.title = "Pipeline sans nom" }
            row.details = ([item.id] + descriptions.map(\.details)).joined(separator: "\n")
            row.character = descriptions.contains(where: \.character)
            return row
        }.sorted { a, b in
            if a.frameUses != b.frameUses { return (a.frameUses ?? 0) > (b.frameUses ?? 0) }
            return a.uses == b.uses ? a.id < b.id : a.uses > b.uses
        }
        applyFilters(userChanged: false)
    }

    private func applyFilters(userChanged: Bool) {
        let query = search.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        visibleChoices = choices.filter { row in
            if characters.state == .on && !row.character { return false }
            if recent.state == .on, let uses = row.frameUses, uses == 0 { return false }
            return query.isEmpty || (row.title + "\n" + row.details).range(of: query, options: [.caseInsensitive, .diacriticInsensitive]) != nil
        }
        // Do not silently retarget the next click as the activity in the scene changes.
        if selection == nil || (userChanged && !visibleChoices.contains(where: { $0.id == selection })) {
            selection = visibleChoices.first?.id
        }
    }

    @objc private func filtersChanged() {
        applyFilters(userChanged: true)
        renderControls()
    }

    func controlTextDidChange(_ obj: Notification) { filtersChanged() }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        if selector == #selector(cancelOperation(_:)) || selector == #selector(insertNewline(_:)) {
            makeFirstResponder(nil)
            resignKey()
            return true
        }
        return false
    }

    private var applied: ShaderFilter {
        guard let inv = inventory, let family = ShaderFilter.Kind(rawValue: inv.kind),
              ["hide", "solo"].contains(inv.mode) else { return ShaderFilter() }
        return ShaderFilter(mode: inv.mode, kind: family, targets: inv.target.components(separatedBy: ","))
    }
    private var desired: ShaderFilter { requested?.filter ?? applied }

    private func brief(_ id: String) -> String {
        id.split(separator: "/").map { "\($0.prefix(7))…\($0.suffix(4))" }.joined(separator: "/")
    }

    private func renderControls() {
        let filter = desired
        let selected = selection.map { filter.contains($0, in: kind) } ?? false
        let row = visibleChoices.first { $0.id == selection }
        scope.stringValue = kind == .pair ? "Agit sur tous les pipelines utilisant cette paire." : "Agit sur une seule configuration de rendu."
        selector.title = row.map { "\($0.caption)  ▾" } ?? (visibleChoices.isEmpty ? "Aucun résultat" : "Choisir un résultat  ▾")
        selector.toolTip = row?.details
        selector.isEnabled = !visibleChoices.isEmpty
        let noun = kind == .pair ? (visibleChoices.count == 1 ? "paire" : "paires") : (visibleChoices.count == 1 ? "pipeline" : "pipelines")
        var activity = ""
        if let row {
            activity = row.frameUses.map { " · \($0) sur l’image" } ?? ""
            activity += " · \(row.uses) au total"
        }
        counts.stringValue = "\(visibleChoices.count) \(noun)\(activity)"
        counts.toolTip = "Pour le choix sélectionné, nombre de dessins rendus ou masqués sur la dernière image inventoriée et depuis le lancement."
        toggle.title = selected && filter.mode == "hide" ? "Rétablir" : "Masquer"
        toggle.isEnabled = row != nil && (filter.mode != "hide" || filter.kind != kind || selected || filter.targets.count < 32)
        let editingSolo = filter.mode == "solo" && filter.kind == kind
        if editingSolo {
            solo.title = selected ? (filter.targets.count == 1 ? "Fin du solo" : "Retirer") : "Ajouter"
        } else {
            solo.title = "Isoler"
        }
        solo.isEnabled = row != nil && (!editingSolo || selected || filter.targets.count < 32)
        reset.isEnabled = desired.mode != "all" || applied.mode != "all"
        var state = "Tous actifs"
        if applied.mode == "hide" {
            let count = applied.targets.count
            let noun = applied.kind == .key ? (count == 1 ? "pipeline masqué" : "pipelines masqués") :
                (count == 1 ? "paire masquée" : "paires masquées")
            state = "\(count) \(noun)"
        }
        if applied.mode == "solo" {
            let targets = applied.targets
            if targets.count == 1 {
                state = "Solo · \(brief(targets[0]))"
            } else {
                state = applied.kind == .key ? "Solo · \(targets.count) pipelines" : "Solo · \(targets.count) paires"
            }
        }
        if inventory == nil { state = "En attente du premier inventaire" }
        if let error = inventory?.error, !error.isEmpty { state = error }
        if requested != nil { state = "Commande en attente du rendu" }
        if let issue { state = issue }
        status.stringValue = state
        status.toolTip = state
        disclosure.title = (expanded ? "▾" : "▸") + "  Shaders A/B" + (expanded || applied.mode == "all" ? "" : " · filtre actif")
    }

    @objc private func switchFamily() { selection = nil; rebuildChoices(); renderControls() }

    @objc private func choose() {
        let menu = NSMenu()
        menu.autoenablesItems = false
        for row in visibleChoices {
            let mark = applied.contains(row.id, in: kind) ? (applied.mode == "hide" ? " · masqué" : " · solo") : ""
            let activity = row.frameUses.map { " · \($0)/image" } ?? ""
            let item = menu.addItem(withTitle: "\(row.caption)\(activity)\(mark)", action: #selector(picked(_:)), keyEquivalent: "")
            item.target = self
            item.toolTip = row.details
            item.representedObject = row.id
            item.state = row.id == selection ? .on : .off
        }
        menu.popUp(positioning: nil, at: NSPoint(x: 0, y: 0), in: selector)
    }

    @objc private func picked(_ sender: NSMenuItem) {
        selection = sender.representedObject as? String
        renderControls()
    }

    @objc private func toggleChoice() { changeChoice(solo: false) }
    @objc private func isolateChoice() { changeChoice(solo: true) }
    private func changeChoice(solo: Bool) {
        guard let id = selection, let next = desired.toggling(id, kind: kind, solo: solo) else { return }
        submit(next)
    }
    @objc private func restoreAll() { submit(ShaderFilter()) }

    private func submit(_ next: ShaderFilter) {
        let url = directory.appendingPathComponent("shader-control.txt")
        do {
            // A restarted HUD must supersede a command still waiting for its first rendered frame.
            if let line = try? String(contentsOf: url, encoding: .utf8),
               let first = line.split(whereSeparator: { $0.isWhitespace }).first, let previous = UInt64(first) {
                revision = max(revision, previous)
            }
            guard revision < UInt64.max else { throw CocoaError(.fileWriteUnknown) }
            revision += 1
            try Data(next.command(revision: revision).utf8).write(to: url, options: .atomic)
            requested = (revision, next)
            issue = nil
        } catch {
            issue = "Écriture de la commande impossible"
        }
        renderControls()
    }
}

@MainActor
private extension ShaderControls {
    static func selfTest(output: URL) throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let panel = ShaderControls(directory: dir)
        let control = dir.appendingPathComponent("shader-control.txt")
        let inventory = dir.appendingPathComponent("shader-inventory.json")
        let a = String(repeating: "2a", count: 16)
        let b = String(repeating: "d4", count: 16)
        let pair = "559a99882abb7171/ffe126abce418399"
        let source = dir.appendingPathComponent("hud.json")
        precondition(Overlay(source: source, parentPID: getppid(), shaderAB: false).shaders == nil)
        precondition(!panel.isVisible && panel.canBecomeKey && panel.becomesKeyOnlyIfNeeded && !panel.canBecomeMain)
        precondition(!panel.selector.isEnabled && !FileManager.default.fileExists(atPath: control.path))
        precondition(!ShaderFilter.Kind.key.accepts(pair) && !ShaderFilter.Kind.pair.accepts(a))
        precondition(!ShaderFilter.Kind.key.accepts(String(repeating: "x", count: 32)))
        func snapshot(_ revision: Int, _ mode: String, _ kind: String, _ targets: String) throws {
            let data: [String: Any] = [
                "version": 1, "frame": 10, "revision": revision, "mode": mode, "kind": kind, "target": targets, "error": "",
                "pipelines": [["key": a, "pair": pair, "drawn": 100, "skipped": 8, "frame_drawn": 2, "frame_skipped": 1],
                              ["key": b, "pair": pair, "drawn": 15, "skipped": 2, "frame_drawn": 0, "frame_skipped": 0]]
            ]
            try JSONSerialization.data(withJSONObject: data).write(to: inventory, options: .atomic)
            // Keep successive test snapshots distinct on filesystems with coarse timestamp resolution.
            try FileManager.default.setAttributes([.modificationDate: Date(timeIntervalSince1970: Double(revision + 100))], ofItemAtPath: inventory.path)
            panel.refreshInventory()
        }
        func assertCommand(_ expected: String) throws {
            let line = try String(contentsOf: control, encoding: .utf8)
            precondition(line == expected + "\n")
        }
        try snapshot(0, "all", "none", "")
        precondition(!panel.expanded && panel.choices.isEmpty)
        panel.fold()
        precondition(panel.recent.state == .on && !panel.visibleChoices.isEmpty)
        precondition(panel.kind == .pair && panel.choices.first?.title == "Paire sans nom")
        let names = ["559a99882abb7171": "MS_VS_PedTransformPreSkinD_6a40bffd_Wrapped",
                     "ffe126abce418399": "PS_DeferredPedTexturedNC_6a40bffd_Wrapped",
                     "0000000000000001": "PS_ResolveWaterSurface_acbd1234_Wrapped"]
        try JSONEncoder().encode(names).write(to: dir.appendingPathComponent("shader-names.json"), options: .atomic)
        panel.refreshInventory()
        precondition(panel.choices.first?.title == "Personnage · matériau texturé")
        let face = ShaderPairName(pair: "f841ad74ec6e7367/6695d7f5e3f6d351", names: [:])
        precondition(face.title == "Visage et cou · peau" && face.character && face.details.contains("Tête"))
        precondition(!ShaderPairName(pair: "0000000000000001/0000000000000001", names: names).character)
        panel.characters.state = .on
        panel.search.stringValue = "materiau"
        panel.filtersChanged()
        precondition(panel.visibleChoices.count == 1)
        panel.search.stringValue = "ffe126abce418399"
        panel.filtersChanged()
        precondition(panel.visibleChoices.count == 1)
        panel.search.stringValue = "inexistant"
        panel.filtersChanged()
        precondition(panel.visibleChoices.isEmpty && !panel.toggle.isEnabled)
        panel.search.stringValue = ""
        panel.characters.state = .off
        panel.family.selectedSegment = 1
        panel.switchFamily()
        panel.recent.state = .on
        panel.filtersChanged()
        precondition(panel.visibleChoices.map(\.id) == [a] && panel.visibleChoices[0].frameUses == 3)
        panel.recent.state = .off
        panel.filtersChanged()
        precondition(panel.choices.map(\.id) == [a, b] && panel.selection == a)
        // Simulate a HUD restart before the renderer has acknowledged the previous file.
        try Data("4 all\n".utf8).write(to: control)
        panel.toggleChoice()
        try assertCommand("5 hide key \(a)")
        precondition(panel.status.stringValue.contains("attente"))
        panel.selection = b
        panel.toggleChoice()
        try assertCommand("6 hide key \(a),\(b)")
        try snapshot(6, "hide", "key", "\(a),\(b)")
        precondition(panel.toggle.title == "Rétablir" && panel.status.stringValue.contains("2"))
        panel.toggleChoice()
        try assertCommand("7 hide key \(a)")
        panel.isolateChoice()
        try assertCommand("8 solo key \(b)")
        panel.isolateChoice()
        try assertCommand("9 all")
        panel.family.selectedSegment = 0
        panel.switchFamily()
        precondition(panel.choices.count == 1 && panel.choices[0].uses == 125)
        panel.toggleChoice()
        try assertCommand("10 hide pair \(pair)")
        try snapshot(10, "hide", "pair", pair)
        precondition(panel.toggle.title == "Rétablir" && panel.reset.isEnabled)
        panel.family.selectedSegment = 1
        panel.switchFamily()
        panel.selection = a
        panel.isolateChoice()
        try assertCommand("11 solo key \(a)")
        panel.selection = b
        panel.isolateChoice()
        try assertCommand("12 solo key \(a),\(b)")
        try snapshot(12, "solo", "key", "\(a),\(b)")
        precondition(panel.solo.title == "Retirer")
        panel.isolateChoice()
        try assertCommand("13 solo key \(a)")
        try snapshot(13, "solo", "key", a)
        precondition(panel.solo.title == "Ajouter")
        panel.fold()
        precondition(panel.selector.isHidden && panel.frame.height == 29)
        panel.fold()
        panel.restoreAll()
        try assertCommand("14 all")
        try snapshot(14, "all", "none", "")
        panel.family.selectedSegment = 0
        panel.switchFamily()
        panel.characters.state = .on
        panel.filtersChanged()
        guard let view = panel.contentView,
              let image = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { throw CocoaError(.coderInvalidValue) }
        view.cacheDisplay(in: view.bounds, to: image)
        guard let png = image.representation(using: .png, properties: [:]) else { throw CocoaError(.fileWriteUnknown) }
        try png.write(to: output, options: .atomic)
        precondition(!panel.reset.isEnabled && !panel.isVisible && !panel.isKeyWindow)
        print("Shaders A/B · recherche, catalogue facultatif, filtres, compteurs, masquage multiple, solo, rétablissement et accusé vérifiés")
    }
}

private struct InputPacket: Decodable {
    enum Source: String, Decodable { case keyboard, xinput, dualsense }
    let v: Int
    let source: Source
    let focus: Bool
    let keys: [UInt8]
    let buttons: UInt16
    let axes: [Int]
}

private struct InputReading: Equatable {
    let source: InputPacket.Source
    let keys: Set<UInt8>
    let buttons: UInt16
    let axes: [Int]
}

private struct InputHistory {
    static let flash = 0.16
    static let timeout = 1.25
    private var last: InputPacket?
    private var updated: TimeInterval = 0
    private var keyFlash: [UInt8: TimeInterval] = [:]
    private var buttonFlash: [UInt16: TimeInterval] = [:]
    private var axisFlash = Array(repeating: (value: 0, until: 0.0), count: 6)

    mutating func receive(_ packet: InputPacket, now: TimeInterval) {
        guard packet.v == 1, packet.axes.count == 6,
              packet.axes.prefix(4).allSatisfy({ (-100...100).contains($0) }),
              packet.axes.suffix(2).allSatisfy({ (0...100).contains($0) }) else { return }
        if !packet.focus || packet.source != last?.source || now - updated >= Self.timeout { self = InputHistory() }
        guard packet.focus else { return }
        let until = now + Self.flash
        for key in packet.keys where !(last?.keys.contains(key) ?? false) { keyFlash[key] = until }
        let pressed = packet.buttons & ~(last?.buttons ?? 0)
        for index in 0..<14 {
            let bit = UInt16(1) << index
            if pressed & bit != 0 { buttonFlash[bit] = until }
        }
        for (index, value) in packet.axes.enumerated() where value != 0 && value != last?.axes[index] {
            axisFlash[index] = (value, until)
        }
        last = packet
        updated = now
    }

    func reading(at now: TimeInterval) -> InputReading? {
        guard let last, now - updated < Self.timeout else { return nil }
        var keys = Set(last.keys)
        for (key, until) in keyFlash where until > now { keys.insert(key) }
        var buttons = last.buttons
        for (bit, until) in buttonFlash where until > now { buttons |= bit }
        var axes = last.axes
        for (i, pulse) in axisFlash.enumerated() where axes[i] == 0 && pulse.until > now { axes[i] = pulse.value }
        return InputReading(source: last.source, keys: keys, buttons: buttons, axes: axes)
    }
}

@MainActor
private final class InputView: NSView {
    static let size = NSSize(width: 402, height: 134)
    var reading: InputReading? {
        didSet { if reading != oldValue { needsDisplay = true } }
    }
    private let accent = NSColor(srgbRed: 0.63, green: 0.89, blue: 0.77, alpha: 1)
    private let cap = NSColor(srgbRed: 0.19, green: 0.21, blue: 0.24, alpha: 1)
    override var isFlipped: Bool { true }

    private var azerty: Bool {
        guard let source = TISCopyCurrentKeyboardLayoutInputSource()?.takeRetainedValue(),
              let property = TISGetInputSourceProperty(source, kTISPropertyInputSourceID) else { return false }
        let name = (Unmanaged<CFString>.fromOpaque(property).takeUnretainedValue() as String).lowercased()
        return name.contains("azerty") || name.contains("belgian") || (name.contains("french") && !name.contains("qwert"))
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let reading else { return }
        let background = NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5), xRadius: 10, yRadius: 10)
        NSColor(srgbRed: 0.055, green: 0.065, blue: 0.078, alpha: 0.92).setFill()
        background.fill()
        NSColor(white: 1, alpha: 0.23).setStroke()
        background.stroke()
        func title(_ string: String, x: CGFloat) {
            (string as NSString).draw(at: NSPoint(x: x, y: 7), withAttributes: [
                .font: NSFont.systemFont(ofSize: 12, weight: .semibold), .foregroundColor: NSColor.white])
        }
        func tile(_ label: String, x: CGFloat, y: CGFloat, width: CGFloat = 16, height: CGFloat = 16,
                  amount: CGFloat, round: Bool = false) {
            let rect = NSRect(x: x, y: y, width: width, height: height)
            let radius = round ? height / 2 : 3
            let path = NSBezierPath(roundedRect: rect, xRadius: radius, yRadius: radius)
            cap.setFill()
            path.fill()
            if amount > 0 {
                NSGraphicsContext.saveGraphicsState()
                path.addClip()
                accent.setFill()
                NSBezierPath(rect: NSRect(x: x, y: rect.maxY - height * amount, width: width, height: height * amount)).fill()
                NSGraphicsContext.restoreGraphicsState()
            }
            let attrs: [NSAttributedString.Key: Any] = [
                .font: NSFont.systemFont(ofSize: 10, weight: .semibold),
                .foregroundColor: amount >= 0.55 ? NSColor.black : NSColor.white]
            let text = label as NSString
            let size = text.size(withAttributes: attrs)
            text.draw(at: NSPoint(x: rect.midX - size.width / 2, y: rect.midY - size.height / 2), withAttributes: attrs)
        }
        title("Clavier", x: 10)
        let padName = reading.source == .dualsense ? "DualSense" : (reading.source == .xinput ? "Manette Xbox" : "Manette émulée")
        title(padName, x: 231)
        func key(_ code: UInt8, _ label: String, _ x: CGFloat, _ y: CGFloat, _ width: CGFloat = 16) {
            tile(label, x: x, y: y, width: width, amount: reading.keys.contains(code) ? 1 : 0)
        }
        key(27, "Esc", 10, 28, 29)
        key(9, "⇥", 43, 28, 29)
        key(8, "⌫", 76, 28, 29)
        key(13, "↵", 109, 28, 29)
        // Use the host keyboard layout when laying out the monitored Windows virtual keys.
        let rows = azerty ? ["AZER Y I", "QSDF HJKL", "WXC B"] : ["QWER Y I", "ASDF HJKL", "ZXC B"]
        for (row, characters) in rows.enumerated() {
            for (col, code) in characters.utf8.enumerated() where code != 32 {
                key(code, String(UnicodeScalar(code)), CGFloat(10 + row * 4 + col * 18), CGFloat(53 + row * 20))
            }
        }
        key(38, "↑", 184, 82)
        key(37, "←", 166, 101)
        key(40, "↓", 184, 101)
        key(39, "→", 202, 101)
        NSColor(white: 1, alpha: 0.18).setFill()
        NSBezierPath(rect: NSRect(x: 224, y: 10, width: 0.5, height: 112)).fill()
        let sony = reading.source == .dualsense
        func button(_ index: Int, _ label: String, _ x: CGFloat, _ y: CGFloat, width: CGFloat = 17, round: Bool = false) {
            tile(label, x: x, y: y, width: width, height: 16, amount: reading.buttons & (UInt16(1) << index) != 0 ? 1 : 0, round: round)
        }
        tile(sony ? "L2" : "LT", x: 233, y: 26, width: 34, height: 14, amount: CGFloat(reading.axes[4]) / 100)
        tile(sony ? "R2" : "RT", x: 359, y: 26, width: 34, height: 14, amount: CGFloat(reading.axes[5]) / 100)
        button(10, sony ? "L1" : "LB", 233, 42, width: 34)
        button(11, sony ? "R1" : "RB", 359, 42, width: 34)
        button(1, sony ? "Cr." : "▣", 277, 42, width: 25)
        button(0, sony ? "Opt." : "≡", 323, 42, width: 27)
        button(6, "↑", 245, 62)
        button(7, "↓", 245, 94)
        button(8, "←", 230, 78)
        button(9, "→", 260, 78)
        button(5, sony ? "△" : "Y", 366, 62, round: true)
        button(2, sony ? "×" : "A", 366, 95, round: true)
        button(4, sony ? "□" : "X", 350, 78, round: true)
        button(3, sony ? "○" : "B", 382, 78, round: true)
        func stick(_ i: Int, x: CGFloat) {
            let center = NSPoint(x: x, y: 110)
            let rim = NSBezierPath(ovalIn: NSRect(x: x - 13, y: center.y - 13, width: 26, height: 26))
            cap.setFill()
            rim.fill()
            let dx = CGFloat(reading.axes[i * 2]) / 100, dy = CGFloat(reading.axes[i * 2 + 1]) / 100
            let scale = 8 / max(1, hypot(dx, dy))
            let click = reading.buttons & (UInt16(1) << (12 + i)) != 0
            (click ? accent : NSColor(white: 0.57, alpha: 1)).setFill()
            let thumb = NSBezierPath(ovalIn: NSRect(x: center.x + dx * scale - 5, y: center.y - dy * scale - 5, width: 10, height: 10))
            thumb.fill()
            if dx != 0 || dy != 0 { accent.setStroke(); thumb.stroke() }
            let label = (sony ? ["L3", "R3"] : ["LS", "RS"])[i] as NSString
            label.draw(at: NSPoint(x: x - 5, y: 122), withAttributes: [
                .font: NSFont.systemFont(ofSize: 8, weight: .medium), .foregroundColor: NSColor.lightGray])
        }
        stick(0, x: 292)
        stick(1, x: 334)
    }
}

@MainActor
private final class InputOverlay: NSPanel {
    let inputView = InputView(frame: NSRect(origin: .zero, size: InputView.size))
    private var history = InputHistory()
    private var anchor: NSPoint?
    private var source: DispatchSourceRead?
    private var pulse: Timer?
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }

    init(descriptor: Int32) {
        super.init(contentRect: NSRect(origin: .zero, size: InputView.size), styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: false)
        isOpaque = false
        backgroundColor = .clear
        isFloatingPanel = true
        level = .floating
        hidesOnDeactivate = false
        ignoresMouseEvents = true
        hasShadow = false
        isExcludedFromWindowsMenu = true
        collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .ignoresCycle]
        contentView = inputView
        let source = DispatchSource.makeReadSource(fileDescriptor: descriptor, queue: .main)
        source.setEventHandler { [weak self] in self?.drain(descriptor) }
        source.setCancelHandler { Darwin.close(descriptor) }
        self.source = source
        source.resume()
    }

    deinit { source?.cancel() }

    func place(at point: NSPoint?) {
        anchor = point
        displayReading()
    }

    private func displayReading() {
        let reading = history.reading(at: ProcessInfo.processInfo.systemUptime)
        inputView.reading = reading
        guard reading != nil, let anchor else {
            if isVisible { orderOut(nil) }
            return
        }
        if frame.origin != anchor { setFrameOrigin(anchor) }
        if !isVisible { orderFrontRegardless() }
    }

    fileprivate func drain(_ fd: Int32) {
        let decoder = JSONDecoder()
        var bytes = [UInt8](repeating: 0, count: 1024)
        // Preserve intervening transitions even when both sides of a tap arrive in the same dispatch.
        for _ in 0..<256 {
            let count = bytes.withUnsafeMutableBytes { Darwin.recv(fd, $0.baseAddress, $0.count, MSG_DONTWAIT) }
            if count < 0 { break }
            guard count > 0, count < bytes.count,
                  let packet = try? decoder.decode(InputPacket.self, from: Data(bytes.prefix(count))) else { continue }
            history.receive(packet, now: ProcessInfo.processInfo.systemUptime)
        }
        displayReading()
        // A single one-shot covers a short tap between the renderer's regular heartbeats.
        if pulse == nil {
            let timer = Timer(timeInterval: InputHistory.flash + 0.01, repeats: false) { [weak self] _ in
                MainActor.assumeIsolated { self?.pulse = nil; self?.displayReading() }
            }
            pulse = timer
            RunLoop.main.add(timer, forMode: .common)
        }
    }
}

private struct WindowGeometry {
    var pid: pid_t
    var bounds: CGRect
}

// Use only CGWindow's required metadata. Titles and window pixels are unnecessary.
private func visibleWindows() -> [WindowGeometry] {
    let options: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
    let windows = CGWindowListCopyWindowInfo(options, kCGNullWindowID) as? [[String: Any]] ?? []
    return windows.compactMap { info in
        guard (info[kCGWindowLayer as String] as? NSNumber)?.intValue == 0,
              let owner = info[kCGWindowOwnerPID as String] as? NSNumber,
              let bounds = info[kCGWindowBounds as String] as? NSDictionary,
              let rect = CGRect(dictionaryRepresentation: bounds),
              rect.width > 1, rect.height > 1,
              ((info[kCGWindowAlpha as String] as? NSNumber)?.doubleValue ?? 1) > 0
        else { return nil }
        return WindowGeometry(pid: owner.int32Value, bounds: rect)
    }
}

private func gameWindow(in windows: [WindowGeometry], pid: pid_t, front: pid_t?) -> CGRect? {
    // Wine may give its application menu to a helper without a window. In that
    // case only accept the frontmost normal window, and match the game's PID.
    if front != pid {
        guard windows.first?.pid == pid, !windows.contains(where: { $0.pid == front }) else { return nil }
    }
    return windows.first { $0.pid == pid && $0.bounds.width >= 240 && $0.bounds.height >= 120 }?.bounds
}

private struct DisplayGeometry {
    let frame: CGRect
    let scale: CGFloat
}

@MainActor
private func panelOrigin(window: CGRect, mainTop: CGFloat, screens: [DisplayGeometry]) -> CGPoint? {
    // Quartz measures Y down from the primary screen. AppKit measures it up.
    let app = CGRect(x: window.minX, y: mainTop - window.maxY, width: window.width, height: window.height)
    guard let screen = screens.max(by: {
        let a = app.intersection($0.frame), b = app.intersection($1.frame)
        return a.width * a.height < b.width * b.height
    }), screen.scale > 0 else { return nil }
    let area = app.intersection(screen.frame)
    let margin = 50 / screen.scale
    guard !area.isNull, area.width >= MetricsView.size.width + margin,
          area.height >= MetricsView.size.height + margin else { return nil }
    return CGPoint(x: area.maxX - margin - MetricsView.size.width,
                   y: area.maxY - margin - MetricsView.size.height)
}

@MainActor
private final class Overlay: NSObject {
    let source: URL
    let parentPID: pid_t
    let panel = GamePanel()
    let shaders: ShaderControls?
    let input: InputOverlay?
    var gamePID: pid_t?
    var timer: Timer?

    init(source: URL, parentPID: pid_t, shaderAB: Bool, inputDescriptor: Int32? = nil) {
        self.source = source
        self.parentPID = parentPID
        shaders = shaderAB ? ShaderControls(directory: source.deletingLastPathComponent()) : nil
        input = inputDescriptor.map { InputOverlay(descriptor: $0) }
        super.init()
        shaders?.didResize = { [weak self] in self?.refresh() }
    }

    func start() {
        let center = NSWorkspace.shared.notificationCenter
        center.addObserver(self, selector: #selector(refresh(_:)), name: NSWorkspace.didActivateApplicationNotification, object: nil)
        center.addObserver(self, selector: #selector(refresh(_:)), name: NSWorkspace.activeSpaceDidChangeNotification, object: nil)
        let tick = Timer(timeInterval: 0.5, target: self, selector: #selector(refresh(_:)), userInfo: nil, repeats: true)
        tick.tolerance = 0.1
        timer = tick
        RunLoop.main.add(tick, forMode: .common)
        refresh()
    }

    @objc func refresh(_ sender: Any? = nil) {
        if getppid() != parentPID { NSApp.terminate(nil); return }
        if let data = try? Data(contentsOf: source),
           let sample = try? JSONDecoder().decode(Sample.self, from: data) {
            if sample.ended == true { NSApp.terminate(nil); return }
            if let pid = sample.game_pid, pid > 1 { gamePID = pid }
            panel.metrics.update(sample)
        }
        guard let pid = gamePID else { return }
        if kill(pid, 0) != 0 && errno == ESRCH { NSApp.terminate(nil); return }
        let screens = NSScreen.screens
        guard let main = screens.first,
              let window = gameWindow(in: visibleWindows(), pid: pid, front: NSWorkspace.shared.frontmostApplication?.processIdentifier),
              let origin = panelOrigin(window: window, mainTop: main.frame.maxY, screens: screens.map { DisplayGeometry(frame: $0.frame, scale: $0.backingScaleFactor) })
        else {
            if panel.isVisible { panel.orderOut(nil) }
            if let shaders, shaders.isVisible { shaders.orderOut(nil) }
            input?.place(at: nil)
            return
        }
        if panel.frame.origin != origin { panel.setFrameOrigin(origin) }
        if !panel.isVisible { panel.orderFrontRegardless() }
        let gameArea = CGRect(x: window.minX, y: main.frame.maxY - window.maxY, width: window.width, height: window.height)
        defer {
          if let input {
            let display = screens.max { a, b in
                let first = gameArea.intersection(a.frame), second = gameArea.intersection(b.frame)
                return first.width * first.height < second.width * second.height
            }
            let area = gameArea.intersection(display?.visibleFrame ?? .null)
            let padding: CGFloat = 12
            let xPositions = [area.maxX - input.frame.width - padding, area.minX + padding]
            let position = xPositions.map { NSRect(x: $0, y: area.minY + padding, width: input.frame.width, height: input.frame.height) }
                .first { rect in !area.isNull && area.contains(rect) && !rect.intersects(panel.frame) &&
                    !(shaders.map { $0.isVisible && rect.intersects($0.frame) } ?? false) }
            input.place(at: position?.origin)
          }
        }
        guard let shaders else { return }
        shaders.refreshInventory()
        let below = NSPoint(x: panel.frame.maxX - shaders.frame.width, y: origin.y - shaders.frame.height - 4)
        let beside = NSPoint(x: origin.x - shaders.frame.width - 4, y: panel.frame.maxY - shaders.frame.height)
        guard let location = [below, beside].first(where: { point in
            let rect = CGRect(origin: point, size: shaders.frame.size)
            return gameArea.contains(rect) && screens.contains { $0.visibleFrame.contains(rect) }
        }) else {
            if shaders.isVisible { shaders.orderOut(nil) }
            return
        }
        if shaders.frame.origin != location { shaders.setFrameOrigin(location) }
        if !shaders.isVisible { shaders.orderFrontRegardless() }
    }
}

private struct HUDOptions {
    var shaderAB = false
    var inputFD: Int32?

    init?(_ args: ArraySlice<String>) {
        var it = args.makeIterator()
        while let option = it.next() {
            if option == "--shader-ab" && !shaderAB { shaderAB = true }
            else if option == "--input-fd", inputFD == nil, let arg = it.next(), let fd = Int32(arg), fd > 2 { inputFD = fd }
            else { return nil }
        }
    }
}

@MainActor
private func selfTestInput(output: URL) throws {
    precondition(HUDOptions(["--input-fd", "7", "--shader-ab"][...])?.shaderAB == true)
    precondition(HUDOptions(["--shader-ab", "--input-fd", "7"][...])?.inputFD == 7)
    precondition(HUDOptions(["--input-fd", "1"][...]) == nil && HUDOptions(["--input-fd"][...]) == nil)
    let pressed = Data(#"{"v":1,"source":"dualsense","focus":true,"keys":[87,68,13],"buttons":1028,"axes":[75,90,0,0,0,84]}"#.utf8)
    let released = Data(#"{"v":1,"source":"dualsense","focus":true,"keys":[],"buttons":0,"axes":[0,0,0,0,0,0]}"#.utf8)
    let idle = Data(#"{"v":1,"source":"dualsense","focus":false,"keys":[],"buttons":0,"axes":[0,0,0,0,0,0]}"#.utf8)
    let decoder = JSONDecoder()
    var history = InputHistory()
    precondition(history.reading(at: 1) == nil)
    history.receive(try decoder.decode(InputPacket.self, from: pressed), now: 1)
    history.receive(try decoder.decode(InputPacket.self, from: released), now: 1.01)
    precondition(history.reading(at: 1.08)?.keys == [87, 68, 13] && history.reading(at: 1.08)?.buttons == 1028)
    precondition(history.reading(at: 1.08)?.axes == [75,90,0,0,0,84])
    precondition(history.reading(at: 1.2)?.keys == [] && history.reading(at: 1.2)?.axes == [0,0,0,0,0,0])
    precondition(history.reading(at: 2.3) == nil)
    history.receive(try decoder.decode(InputPacket.self, from: idle), now: 1.1)
    precondition(history.reading(at: 1.1) == nil)
    let rx = socket(AF_INET, SOCK_DGRAM, 0), tx = socket(AF_INET, SOCK_DGRAM, 0)
    guard rx >= 0 && tx >= 0 else { throw CocoaError(.fileReadUnknown) }
    defer { Darwin.close(tx) }
    var address = sockaddr_in()
    address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
    address.sin_family = sa_family_t(AF_INET)
    address.sin_addr.s_addr = inet_addr("127.0.0.1")
    var length = socklen_t(MemoryLayout<sockaddr_in>.size)
    let overlay = InputOverlay(descriptor: rx)
    precondition(!overlay.canBecomeKey && !overlay.canBecomeMain && overlay.ignoresMouseEvents)
    try withUnsafeMutablePointer(to: &address) { local in
        try local.withMemoryRebound(to: sockaddr.self, capacity: 1) { ptr in
            guard Darwin.bind(rx, ptr, length) == 0, getsockname(rx, ptr, &length) == 0 else { throw CocoaError(.fileReadUnknown) }
            for data in [pressed, released] {
                let sent = data.withUnsafeBytes { sendto(tx, $0.baseAddress, $0.count, 0, ptr, length) }
                guard sent == data.count else { throw CocoaError(.fileWriteUnknown) }
            }
        }
    }
    var ready = pollfd(fd: rx, events: Int16(POLLIN), revents: 0)
    guard poll(&ready, 1, 1_000) == 1 else { throw CocoaError(.fileReadUnknown) }
    overlay.drain(rx)
    let view = overlay.inputView
    precondition(view.reading?.keys == [87, 68, 13], "touches UDP \(String(describing: view.reading))")
    precondition(view.reading?.buttons == 1028 && !overlay.isVisible, "boutons ou visibilité sans point d’ancrage")
    guard let bitmap = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { throw CocoaError(.coderInvalidValue) }
    view.cacheDisplay(in: view.bounds, to: bitmap)
    guard let png = bitmap.representation(using: .png, properties: [:]) else { throw CocoaError(.fileWriteUnknown) }
    try png.write(to: output, options: .atomic)
    print("HUD entrées · UDP local, clavier simultané, DualSense, axes, gâchette, tap, perte de focus et expiration vérifiés")
}

@MainActor
private func selfTest(output: URL) throws {
    let panel = GamePanel()
    precondition(!panel.canBecomeKey && !panel.canBecomeMain && panel.ignoresMouseEvents)
    precondition(panel.collectionBehavior.contains([.canJoinAllSpaces, .fullScreenAuxiliary]))
    let sample = try JSONDecoder().decode(Sample.self, from: Data("""
        {"game_pid":123,"fps":0.117647,"frame_ms":8500.0,"ram_mib":6254.2,"cpu_percent":174.2,"gpu_ms":1245.0,"copy_ms":6380.5,"wired_mib":8100,"updated":1790000000}
        """.utf8))
    panel.metrics.update(sample)
    let values = panel.metrics.values
    precondition(values.fps == "0,12" && values.ram == "6,11 Gio" && values.cpu == "174 %")
    precondition(values.frame == milliseconds(8500) && values.gpu == milliseconds(1245) && values.copies == milliseconds(6380.5))
    precondition(milliseconds(16.67) == "16,7 ms" && milliseconds(nil) == "—" && Readings(Sample()).cpu == "—")
    precondition(fpsText(1.0 / 30) == "0,033" && fpsText(nil) == "—" && ramText(nil) == "—")
    let target = CGRect(x: 120, y: 45, width: 900, height: 500)
    let game = WindowGeometry(pid: 123, bounds: target), other = WindowGeometry(pid: 456, bounds: target)
    precondition(gameWindow(in: [game, other], pid: 123, front: 123) == target)
    precondition(gameWindow(in: [other, game], pid: 123, front: 456) == nil)
    precondition(gameWindow(in: [game, other], pid: 123, front: 456) == nil)
    precondition(gameWindow(in: [game], pid: 123, front: 789) == target)
    let screen = CGRect(x: 0, y: 0, width: 1440, height: 900)
    for scale: CGFloat in [1, 2] {
        let origin = panelOrigin(window: target, mainTop: 900, screens: [DisplayGeometry(frame: screen, scale: scale)])!
        precondition((target.maxX - origin.x - MetricsView.size.width) * scale == 50)
        precondition((900 - target.minY - origin.y - MetricsView.size.height) * scale == 50)
    }
    let view = panel.metrics
    guard let bitmap = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { throw CocoaError(.coderInvalidValue) }
    view.cacheDisplay(in: view.bounds, to: bitmap)
    guard let png = bitmap.representation(using: .png, properties: [:]) else { throw CocoaError(.fileWriteUnknown) }
    try png.write(to: output, options: .atomic)
    print("mirakud | FPS \(values.fps) | CPU Victory \(values.cpu) | RAM Victory \(values.ram) | Image \(values.frame) | Attente GPU \(values.gpu) | Copies \(values.copies) | \(bitmap.pixelsWide) × \(bitmap.pixelsHigh)")
}

private final class SonyPresence {
    private let url: URL
    private var devices = Set<UInt>()
    private var published: Bool?
    private var failure: Error?

    init(_ url: URL) { self.url = url }

    private func publish() throws {
        let present = !devices.isEmpty
        guard published != present else { return }
        try Data((present ? "1\n" : "0\n").utf8).write(to: url, options: .atomic)
        published = present
    }

    private func change(_ id: UInt, present: Bool) throws {
        if present { devices.insert(id) } else { devices.remove(id) }
        try publish()
    }

    private func event(_ result: IOReturn, _ device: IOHIDDevice, present: Bool) {
        do {
            guard result == kIOReturnSuccess else { throw NSError(domain: NSOSStatusErrorDomain, code: Int(result)) }
            try change(UInt(bitPattern: Unmanaged.passUnretained(device).toOpaque()), present: present)
        } catch {
            failure = error
            CFRunLoopStop(CFRunLoopGetMain())
        }
    }

    func watch() throws {
        // Independent devices: the manager sees arrivals and removals without
        // opening devices or receiving their input reports. Transport is unrestricted.
        let manager = IOHIDManagerCreate(kCFAllocatorDefault, IOHIDManagerOptions.independentDevices.rawValue)
        let matches = [0x0ce6, 0x0df2].map { [kIOHIDVendorIDKey: 0x054c, kIOHIDProductIDKey: $0] }
        IOHIDManagerSetDeviceMatchingMultiple(manager, matches as CFArray)
        let context = Unmanaged.passUnretained(self).toOpaque()
        IOHIDManagerRegisterDeviceMatchingCallback(manager, { ctx, result, _, device in
            Unmanaged<SonyPresence>.fromOpaque(ctx!).takeUnretainedValue().event(result, device, present: true)
        }, context)
        IOHIDManagerRegisterDeviceRemovalCallback(manager, { ctx, result, _, device in
            Unmanaged<SonyPresence>.fromOpaque(ctx!).takeUnretainedValue().event(result, device, present: false)
        }, context)
        let loop = CFRunLoopGetMain()!
        IOHIDManagerScheduleWithRunLoop(manager, loop, CFRunLoopMode.defaultMode.rawValue)
        defer { IOHIDManagerUnscheduleFromRunLoop(manager, loop, CFRunLoopMode.defaultMode.rawValue) }
        let result = IOHIDManagerOpen(manager, 0)
        guard result == kIOReturnSuccess else { throw NSError(domain: NSOSStatusErrorDomain, code: Int(result)) }
        defer { IOHIDManagerClose(manager, 0) }
        if let found = IOHIDManagerCopyDevices(manager) {
            var items = [UnsafeRawPointer?](repeating: nil, count: CFSetGetCount(found))
            CFSetGetValues(found, &items)
            devices = Set(items.compactMap { $0.map { UInt(bitPattern: $0) } })
        }
        try publish()
        // A closed pipe also ends this helper if the launching Python process exits.
        let lifetime = DispatchSource.makeReadSource(fileDescriptor: STDIN_FILENO, queue: .main)
        lifetime.setEventHandler { CFRunLoopStop(CFRunLoopGetMain()) }
        lifetime.resume()
        defer { lifetime.cancel() }
        FileHandle.standardOutput.write(Data("ready\n".utf8))
        withExtendedLifetime(manager) { CFRunLoopRun() }
        if let failure { throw failure }
    }

    static func selfTest() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = dir.appendingPathComponent("sony-present")
        let observer = SonyPresence(file)
        try observer.publish()
        func check(_ expected: String) throws {
            let contents = try String(contentsOf: file, encoding: .utf8)
            precondition(contents == expected + "\n")
        }
        try check("0")
        try observer.change(10, present: true)
        try check("1")
        try observer.change(10, present: true)
        try observer.change(20, present: true)
        try observer.change(10, present: false)
        try check("1")
        try observer.change(20, present: false)
        try check("0")
        try observer.change(10, present: true)
        try check("1")
        print("Sony · absence, connexion, doublon, deux périphériques, retrait et reconnexion vérifiés")
    }
}

let helperArgs = CommandLine.arguments
if helperArgs.count == 3 && helperArgs[1] == "--watch-dualsense" && helperArgs[2].hasPrefix("/") {
    do { try SonyPresence(URL(fileURLWithPath: helperArgs[2])).watch() }
    catch { FileHandle.standardError.write(Data("Détection Sony indisponible : \(error)\n".utf8)); exit(EXIT_FAILURE) }
    exit(EXIT_SUCCESS)
} else if helperArgs.count == 2 && helperArgs[1] == "--self-test-dualsense" {
    try SonyPresence.selfTest()
    exit(EXIT_SUCCESS)
}

try MainActor.assumeIsolated {
    let initialParent = getppid()
    let app = NSApplication.shared
    app.setActivationPolicy(.accessory)
    let args = CommandLine.arguments
    if args.count == 3, args[1] == "--self-test", args[2].hasPrefix("/") {
        try selfTest(output: URL(fileURLWithPath: args[2]))
    } else if args.count == 3, args[1] == "--self-test-shaders", args[2].hasPrefix("/") {
        try ShaderControls.selfTest(output: URL(fileURLWithPath: args[2]))
    } else if args.count == 3, args[1] == "--self-test-input", args[2].hasPrefix("/") {
        try selfTestInput(output: URL(fileURLWithPath: args[2]))
    } else if args.count >= 2, args[1].hasPrefix("/"), let options = HUDOptions(args.dropFirst(2)) {
        let overlay = Overlay(source: URL(fileURLWithPath: args[1]), parentPID: initialParent, shaderAB: options.shaderAB, inputDescriptor: options.inputFD)
        overlay.start()
        withExtendedLifetime(overlay) { app.run() }
    } else {
        FileHandle.standardError.write(Data("Usage: victory-perf-hud /chemin/mesures.json [--shader-ab] [--input-fd N] | --self-test[-shaders|-input] /chemin/encart.png\n".utf8))
        exit(64)
    }
}
