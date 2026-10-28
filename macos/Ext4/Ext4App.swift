import AppKit
import FSKit
import OSLog
import SwiftUI

private let logger = Logger(subsystem: "dev.watzon.macext4", category: "app")

@main
struct Ext4App: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = SetupModel()

    var body: some Scene {
        Window("Ext4", id: "main") {
            MainView()
                .environmentObject(model)
                .frame(width: 560)
                .fixedSize(horizontal: false, vertical: true)
        }
        .windowResizability(.contentSize)
        .windowStyle(.hiddenTitleBar)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

// MARK: - Model

enum StepState: Equatable {
    case checking, done, needsAction, optional
}

struct ExtVolume: Identifiable, Equatable {
    let url: URL
    let name: String
    let format: String
    let capacity: Int64
    let available: Int64
    let readOnly: Bool
    var id: URL { url }
}

@MainActor
final class SetupModel: ObservableObject {
    static let moduleID = "dev.watzon.macext4.fsmodule"
    static let nameBundle = URL(fileURLWithPath: "/Library/Filesystems/ext4.fs")
    static let cliPaths = ["/usr/local/bin/ext4", "/opt/homebrew/bin/ext4"]

    @Published var moduleInstalled: StepState = .checking
    @Published var moduleEnabled: StepState = .checking
    @Published var finderNames: StepState = .checking
    @Published var commandLine: StepState = .checking
    @Published var volumes: [ExtVolume] = []
    @Published var lastError: String?

    private var observers: [NSObjectProtocol] = []

    init() {
        let center = NSWorkspace.shared.notificationCenter
        for name in [NSWorkspace.didMountNotification, NSWorkspace.didUnmountNotification, NSWorkspace.didRenameVolumeNotification] {
            observers.append(center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                Task { @MainActor in self?.refreshVolumes() }
            })
        }
        observers.append(NotificationCenter.default.addObserver(forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main) { [weak self] _ in
            Task { @MainActor in self?.refresh() }
        })
        refresh()
    }

    var setupComplete: Bool {
        moduleInstalled == .done && moduleEnabled == .done
    }

    func refresh() {
        let fm = FileManager.default
        finderNames = fm.fileExists(atPath: Self.nameBundle.appendingPathComponent("Contents/Info.plist").path) ? .done : .needsAction
        commandLine = Self.cliPaths.contains(where: fm.isExecutableFile(atPath:)) ? .done : .optional
        refreshModule()
        refreshVolumes()
    }

    private func refreshModule() {
        FSClient.shared.fetchInstalledExtensions { identities, error in
            Task { @MainActor in
                if let error {
                    logger.error("fetchInstalledExtensions failed: \(error.localizedDescription, privacy: .public)")
                    self.lastError = error.localizedDescription
                    self.moduleInstalled = .needsAction
                    self.moduleEnabled = .needsAction
                    return
                }
                let module = identities?.first { $0.bundleIdentifier == Self.moduleID }
                logger.notice("module installed=\(module != nil, privacy: .public) enabled=\(module?.isEnabled ?? false, privacy: .public)")
                self.lastError = nil
                self.moduleInstalled = module == nil ? .needsAction : .done
                self.moduleEnabled = module?.isEnabled == true ? .done : .needsAction
            }
        }
    }

    func refreshVolumes() {
        let keys: [URLResourceKey] = [
            .volumeNameKey, .volumeTypeNameKey, .volumeLocalizedFormatDescriptionKey,
            .volumeTotalCapacityKey, .volumeAvailableCapacityKey, .volumeIsReadOnlyKey,
        ]
        let urls = FileManager.default.mountedVolumeURLs(includingResourceValuesForKeys: keys, options: [.skipHiddenVolumes]) ?? []
        volumes = urls.compactMap { url in
            guard let v = try? url.resourceValues(forKeys: Set(keys)),
                  let type = v.volumeTypeName, ["ext2", "ext3", "ext4"].contains(type) else { return nil }
            let format = v.volumeLocalizedFormatDescription.flatMap { $0.hasPrefix("Unknown") ? nil : $0 } ?? "Linux \(type)"
            return ExtVolume(
                url: url,
                name: v.volumeName ?? url.lastPathComponent,
                format: format,
                capacity: Int64(v.volumeTotalCapacity ?? 0),
                available: Int64(v.volumeAvailableCapacity ?? 0),
                readOnly: v.volumeIsReadOnly ?? false
            )
        }
        .sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
    }

    func openExtensionSettings() {
        if #available(macOS 27.0, *), FSClient.shared.openFileSystemExtensionsSettings() {
            return
        }
        if let url = URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension") {
            NSWorkspace.shared.open(url)
        }
    }

    func eject(_ volume: ExtVolume) {
        do {
            try NSWorkspace.shared.unmountAndEjectDevice(at: volume.url)
        } catch {
            lastError = "Could not eject \(volume.name): \(error.localizedDescription)"
        }
        refreshVolumes()
    }
}

// MARK: - Views

private struct MainView: View {
    @EnvironmentObject private var model: SetupModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Header(complete: model.setupComplete)
                .padding(.horizontal, 28)
                .padding(.top, 28)
                .padding(.bottom, 20)

            Divider()

            VStack(alignment: .leading, spacing: 14) {
                SectionTitle("Setup")
                StepRow(
                    state: model.moduleInstalled,
                    title: "File system module installed",
                    detail: model.moduleInstalled == .done
                        ? "macOS has registered the Ext4 module."
                        : "macOS does not list the Ext4 module yet. Reinstall Ext4, then open this window again."
                )
                StepRow(
                    state: model.moduleEnabled,
                    title: "Module turned on",
                    detail: model.moduleEnabled == .done
                        ? "Drives with ext2, ext3, or ext4 partitions mount when you connect them."
                        : "Turn on Ext4 once in System Settings. Until then, macOS ignores ext drives."
                ) {
                    if model.moduleEnabled != .done {
                        Button("Open System Settings") { model.openExtensionSettings() }
                            .controlSize(.regular)
                    }
                }
                if model.moduleEnabled == .needsAction {
                    EnableInstructions()
                        .padding(.leading, 30)
                }
                StepRow(
                    state: model.finderNames,
                    title: "Finder format name",
                    detail: model.finderNames == .done
                        ? "Finder shows ext drives as \"Linux ext4\"."
                        : "Finder shows \"Unknown (ext4)\". Reinstall Ext4 to add the name."
                )
                StepRow(
                    state: model.commandLine,
                    title: "Command-line tool",
                    detail: model.commandLine == .done
                        ? "Use `ext4` in Terminal to format, check, mount, and unmount drives."
                        : "Optional. The installer puts `ext4` in /usr/local/bin."
                )
            }
            .padding(.horizontal, 28)
            .padding(.vertical, 20)

            Divider()

            VStack(alignment: .leading, spacing: 12) {
                HStack {
                    SectionTitle("Drives")
                    Spacer()
                    Button {
                        model.refresh()
                    } label: {
                        Label("Refresh", systemImage: "arrow.clockwise")
                    }
                    .buttonStyle(.borderless)
                    .help("Check setup and drives again")
                }
                if model.volumes.isEmpty {
                    Text("No ext2, ext3, or ext4 drives are mounted. Connect one to use it in Finder.")
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                } else {
                    ForEach(model.volumes) { volume in
                        VolumeRow(volume: volume)
                    }
                }
                FormatHelp()
            }
            .padding(.horizontal, 28)
            .padding(.vertical, 20)

            if let error = model.lastError {
                Divider()
                Label(error, systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
                    .padding(.horizontal, 28)
                    .padding(.vertical, 12)
            }
        }
    }
}

private struct Header: View {
    let complete: Bool

    var body: some View {
        HStack(alignment: .center, spacing: 16) {
            Image(nsImage: NSApp.applicationIconImage)
                .resizable()
                .frame(width: 56, height: 56)
            VStack(alignment: .leading, spacing: 4) {
                Text("Ext4 for Mac")
                    .font(.title2.weight(.semibold))
                Text(complete
                     ? "Ready. Connect a Linux drive and it opens in Finder."
                     : "Read and write Linux ext2, ext3, and ext4 drives. Finish setup below.")
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
        }
    }
}

private struct SectionTitle: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .font(.headline)
    }
}

private struct StepRow<Accessory: View>: View {
    let state: StepState
    let title: String
    let detail: String
    @ViewBuilder var accessory: () -> Accessory

    init(state: StepState, title: String, detail: String, @ViewBuilder accessory: @escaping () -> Accessory = { EmptyView() }) {
        self.state = state
        self.title = title
        self.detail = detail
        self.accessory = accessory
    }

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            StateIcon(state: state)
                .frame(width: 18, height: 18)
                .padding(.top, 1)
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .fontWeight(.medium)
                Text(LocalizedStringKey(detail))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 8)
            accessory()
        }
    }
}

private struct StateIcon: View {
    let state: StepState

    var body: some View {
        switch state {
        case .checking:
            ProgressView().controlSize(.small)
        case .done:
            Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        case .needsAction:
            Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.orange)
        case .optional:
            Image(systemName: "circle.dashed").foregroundStyle(.secondary)
        }
    }
}

private struct EnableInstructions: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("In System Settings:")
                .font(.callout.weight(.medium))
            InstructionLine(number: 1, text: "Open **General > Login Items & Extensions**.")
            InstructionLine(number: 2, text: "Under Extensions, switch the list to **By Category**.")
            InstructionLine(number: 3, text: "Click ⓘ next to **File System Extensions** and turn on **Ext4**.")
            Text("The switch in the \"By App\" list does not respond on current macOS versions. Use the category list.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.top, 2)
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.secondary.opacity(0.08)))
    }
}

private struct InstructionLine: View {
    let number: Int
    let text: String

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Text("\(number).")
                .monospacedDigit()
                .foregroundStyle(.secondary)
            Text(LocalizedStringKey(text))
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(.callout)
    }
}

private struct VolumeRow: View {
    @EnvironmentObject private var model: SetupModel
    let volume: ExtVolume

    var body: some View {
        HStack(spacing: 12) {
            Image(nsImage: NSWorkspace.shared.icon(forFile: volume.url.path))
                .resizable()
                .frame(width: 32, height: 32)
            VStack(alignment: .leading, spacing: 2) {
                Text(volume.name)
                    .fontWeight(.medium)
                Text(summary)
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 8)
            Button("Show in Finder") {
                NSWorkspace.shared.activateFileViewerSelecting([volume.url])
            }
            Button("Eject") { model.eject(volume) }
        }
    }

    private var summary: String {
        let free = ByteCountFormatter.string(fromByteCount: volume.available, countStyle: .file)
        let total = ByteCountFormatter.string(fromByteCount: volume.capacity, countStyle: .file)
        var parts = [volume.format, "\(free) available of \(total)"]
        if volume.readOnly { parts.append("read-only") }
        return parts.joined(separator: " · ")
    }
}

private struct FormatHelp: View {
    @State private var expanded = false
    private let command = "diskutil eraseVolume ext4 NAME diskNsM"

    var body: some View {
        DisclosureGroup("Format a drive as ext4", isExpanded: $expanded) {
            VStack(alignment: .leading, spacing: 8) {
                Text("In Disk Utility, select the drive or partition, click Erase, and choose **Linux ext4** as the format. Erasing a whole drive with the GUID scheme also marks the partition as a Linux file system. From Terminal, find the partition with `diskutil list` and run this command. Both erase everything on it.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                HStack {
                    Text(command)
                        .font(.system(.callout, design: .monospaced))
                        .textSelection(.enabled)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 5)
                        .background(RoundedRectangle(cornerRadius: 6).fill(Color.secondary.opacity(0.1)))
                    Button("Copy") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(command, forType: .string)
                    }
                }
            }
            .padding(.top, 6)
        }
        .font(.callout)
        .padding(.top, 4)
    }
}
