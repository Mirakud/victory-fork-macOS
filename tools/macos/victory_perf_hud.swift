// swiftc -O victory_perf_hud.swift -o victory-perf-hud
// victory-perf-hud /absolute/path/to/telemetry.json
// victory-perf-hud --self-test /absolute/path/to/snapshot.png

import AppKit
import CoreGraphics
import Darwin

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

@MainActor
private final class ShaderControls: NSPanel {
    static let width: CGFloat = 300
    private let directory: URL
    private let disclosure = NSButton()
    private let family = NSSegmentedControl(labels: ["Pipeline", "Paire de shaders"], trackingMode: .selectOne, target: nil, action: nil)
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
    private var choices: [(id: String, uses: UInt64)] = []
    private var selection: String?
    private var expanded = true
    var didResize: (() -> Void)?
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
    private var kind: ShaderFilter.Kind { family.selectedSegment == 1 ? .pair : .key }

    init(directory: URL) {
        self.directory = directory
        super.init(contentRect: NSRect(x: 0, y: 0, width: Self.width, height: 145),
                   styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: false)
        isOpaque = false
        backgroundColor = .clear
        isFloatingPanel = true
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
        selector.action = #selector(choose)
        selector.setAccessibilityLabel("Sélectionner le pipeline ou la paire de shaders")
        toggle.action = #selector(toggleChoice)
        solo.action = #selector(isolateChoice)
        reset.title = "Tout rétablir"
        reset.action = #selector(restoreAll)
        status.font = .systemFont(ofSize: 11)
        status.textColor = .secondaryLabelColor
        status.lineBreakMode = .byTruncatingTail
        view.addSubview(status)
        layoutControls()
        renderControls()
    }

    private func layoutControls() {
        let height: CGFloat = expanded ? 145 : 29
        setContentSize(NSSize(width: Self.width, height: height))
        func place(_ item: NSView, _ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat) {
            item.frame = NSRect(x: x, y: height - y - h, width: w, height: h)
        }
        place(disclosure, 8, 2, 282, 25)
        place(family, 9, 29, 282, 23)
        place(selector, 9, 53, 282, 25)
        place(toggle, 9, 83, 80, 25)
        place(solo, 91, 83, 80, 25)
        place(reset, 173, 83, 118, 25)
        place(status, 12, 115, 276, 19)
        for item in [family, selector, toggle, solo, reset, status] { item.isHidden = !expanded }
        disclosure.title = (expanded ? "▾" : "▸") + "  Shaders A/B"
        disclosure.setAccessibilityLabel(expanded ? "Replier les contrôles shaders" : "Déplier les contrôles shaders")
    }

    @objc private func fold() {
        expanded.toggle()
        layoutControls()
        didResize?()
    }

    func refreshInventory() {
        let url = directory.appendingPathComponent("shader-inventory.json")
        guard let date = try? url.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate,
              date != inventoryDate else { return }
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
        rebuildChoices()
        renderControls()
    }

    private func rebuildChoices() {
        var counts: [String: UInt64] = [:]
        for item in inventory?.pipelines ?? [] {
            let id = kind == .key ? item.key : item.pair
            if kind.accepts(id) { counts[id, default: 0] += item.drawn + item.skipped }
        }
        choices = counts.map { ($0.key, $0.value) }.sorted { a, b in
            a.uses == b.uses ? a.id < b.id : a.uses > b.uses
        }
        if selection.flatMap({ counts[$0] }) == nil { selection = choices.first?.id }
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
        let row = choices.first { $0.id == selection }
        selector.title = row.map { "\(brief($0.id)) · \($0.uses) utilis.  ▾" } ?? "Aucun pipeline observé"
        selector.toolTip = selection
        selector.isEnabled = row != nil
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
        for row in choices {
            let mark = applied.contains(row.id, in: kind) ? (applied.mode == "hide" ? " · masqué" : " · solo") : ""
            let item = menu.addItem(withTitle: "\(row.id)  ·  \(row.uses) utilis.\(mark)", action: #selector(picked(_:)), keyEquivalent: "")
            item.target = self
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
        let pair = String(repeating: "39", count: 8) + "/" + String(repeating: "ab", count: 8)
        let source = dir.appendingPathComponent("hud.json")
        precondition(Overlay(source: source, parentPID: getppid(), shaderAB: false).shaders == nil)
        precondition(!panel.isVisible && !panel.canBecomeKey && !panel.canBecomeMain)
        precondition(!panel.selector.isEnabled && !FileManager.default.fileExists(atPath: control.path))
        precondition(!ShaderFilter.Kind.key.accepts(pair) && !ShaderFilter.Kind.pair.accepts(a))
        precondition(!ShaderFilter.Kind.key.accepts(String(repeating: "x", count: 32)))
        func snapshot(_ revision: Int, _ mode: String, _ kind: String, _ targets: String) throws {
            let data: [String: Any] = [
                "version": 1, "frame": 10, "revision": revision, "mode": mode, "kind": kind, "target": targets, "error": "",
                "pipelines": [["key": a, "pair": pair, "drawn": 100, "skipped": 8],
                              ["key": b, "pair": pair, "drawn": 15, "skipped": 2]]
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
        panel.family.selectedSegment = 1
        panel.switchFamily()
        precondition(panel.choices.count == 1 && panel.choices[0].uses == 125)
        panel.toggleChoice()
        try assertCommand("10 hide pair \(pair)")
        try snapshot(10, "hide", "pair", pair)
        precondition(panel.toggle.title == "Rétablir" && panel.reset.isEnabled)
        panel.family.selectedSegment = 0
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
        guard let view = panel.contentView,
              let image = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { throw CocoaError(.coderInvalidValue) }
        view.cacheDisplay(in: view.bounds, to: image)
        guard let png = image.representation(using: .png, properties: [:]) else { throw CocoaError(.fileWriteUnknown) }
        try png.write(to: output, options: .atomic)
        panel.restoreAll()
        try assertCommand("14 all")
        try snapshot(14, "all", "none", "")
        precondition(!panel.reset.isEnabled && !panel.isVisible && !panel.isKeyWindow)
        print("Shaders A/B · 2 pipelines, 1 paire · masquage multiple, solo, rétablissement et accusé vérifiés")
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
    var gamePID: pid_t?
    var timer: Timer?

    init(source: URL, parentPID: pid_t, shaderAB: Bool) {
        self.source = source
        self.parentPID = parentPID
        shaders = shaderAB ? ShaderControls(directory: source.deletingLastPathComponent()) : nil
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
            return
        }
        if panel.frame.origin != origin { panel.setFrameOrigin(origin) }
        if !panel.isVisible { panel.orderFrontRegardless() }
        guard let shaders else { return }
        shaders.refreshInventory()
        let below = NSPoint(x: origin.x, y: origin.y - shaders.frame.height - 4)
        let beside = NSPoint(x: origin.x - shaders.frame.width - 4, y: panel.frame.maxY - shaders.frame.height)
        let gameArea = CGRect(x: window.minX, y: main.frame.maxY - window.maxY, width: window.width, height: window.height)
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

try MainActor.assumeIsolated {
    let initialParent = getppid()
    let app = NSApplication.shared
    app.setActivationPolicy(.accessory)
    let args = CommandLine.arguments
    if args.count == 3, args[1] == "--self-test", args[2].hasPrefix("/") {
        try selfTest(output: URL(fileURLWithPath: args[2]))
    } else if args.count == 3, args[1] == "--self-test-shaders", args[2].hasPrefix("/") {
        try ShaderControls.selfTest(output: URL(fileURLWithPath: args[2]))
    } else if (args.count == 2 || (args.count == 3 && args[2] == "--shader-ab")), args[1].hasPrefix("/") {
        let overlay = Overlay(source: URL(fileURLWithPath: args[1]), parentPID: initialParent, shaderAB: args.count == 3)
        overlay.start()
        withExtendedLifetime(overlay) { app.run() }
    } else {
        FileHandle.standardError.write(Data("Usage: victory-perf-hud /chemin/mesures.json [--shader-ab] | --self-test /chemin/encart.png | --self-test-shaders /chemin/panneau.png\n".utf8))
        exit(64)
    }
}
