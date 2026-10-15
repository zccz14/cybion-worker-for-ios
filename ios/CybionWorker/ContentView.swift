import SwiftUI

struct ContentView: View {
    @StateObject private var model = WorkerModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Toggle(String(localized: "enable_worker"), isOn: Binding(
                        get: { model.enabled },
                        set: { model.setEnabled($0) }
                    ))
                } footer: {
                    Text(String(localized: "enable_footer"))
                }

                Section(String(localized: "section_status")) {
                    ForEach(model.statusLines, id: \.self) { line in
                        Text(line)
                    }
                }

                if let code = model.pairingCode {
                    Section(String(localized: "section_pairing")) {
                        Text(code)
                            .font(.system(.title2, design: .monospaced))
                            .textSelection(.enabled)
                        Button(String(localized: "open_pairing")) {
                            model.openPairingPage()
                        }
                        Text(String(localized: "pairing_hint"))
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }
                }

                if model.canRetry {
                    Section {
                        Button(String(localized: "retry")) { model.retry() }
                    }
                }

                if model.canReset {
                    Section {
                        Button(String(localized: "reset_pairing"), role: .destructive) {
                            model.reset()
                        }
                        Text(String(localized: "reset_hint"))
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }
                }

                Section(String(localized: "section_limits")) {
                    LabeledContent(String(localized: "keep_alive"), value: model.keepAliveActive
                        ? String(localized: "keep_alive_on")
                        : String(localized: "keep_alive_off"))
                    Text(String(localized: "limits_footer"))
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
            }
            .navigationTitle(String(localized: "app_title"))
            Section {
                Text(String(format: String(localized: "footer_version"), model.version))
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .center)
                    .listRowBackground(Color.clear)
            }
        }
        .onAppear { model.begin() }
        .onDisappear { model.end() }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { model.tick() }
        }
    }
}
