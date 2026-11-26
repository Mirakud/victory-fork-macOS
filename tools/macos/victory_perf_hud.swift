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
    var gamePID: pid_t?
    var timer: Timer?

    init(source: URL, parentPID: pid_t) {
        self.source = source
        self.parentPID = parentPID
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
            return
        }
        if panel.frame.origin != origin { panel.setFrameOrigin(origin) }
        if !panel.isVisible { panel.orderFrontRegardless() }
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
    } else if args.count == 2, args[1].hasPrefix("/") {
        let overlay = Overlay(source: URL(fileURLWithPath: args[1]), parentPID: initialParent)
        overlay.start()
        withExtendedLifetime(overlay) { app.run() }
    } else {
        FileHandle.standardError.write(Data("Usage: victory-perf-hud /chemin/mesures.json | --self-test /chemin/encart.png\n".utf8))
        exit(64)
    }
}
