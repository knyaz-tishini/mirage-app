import Foundation
import MirageTun
import NetworkExtension
import SwiftUI

#if os(iOS)
import UIKit
#else
import AppKit
#endif

private let cabinetURL = URL(string: "https://m1r4g3.bid/cabinet/")!
private let tunnelBundle = "bid.m1r4g3.tunnel"

struct ConnectView: View {
    @State private var code = ""
    @State private var status = "не подключено"
    @State private var busy = false
    @State private var bound = DeviceStore.exists
    @State private var showBind = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("m1r4g3")
                .font(.title)
            Text("коннектор к сети")
                .foregroundStyle(.secondary)
            Text(status)
            if bound {
                Button("ОТВЯЗАТЬ") { unlink() }
                    .disabled(busy)
            } else {
                Button("ПРИВЯЗАТЬ") { showBind = true }
                    .disabled(busy)
            }
            Button("КАБИНЕТ") { openCabinet() }
        }
        .padding(24)
        .frame(maxWidth: 420, alignment: .leading)
        .onAppear { refreshStatus() }
        .sheet(isPresented: $showBind) { bindSheet }
    }

    private var bindSheet: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("привязка к кабинету")
                .font(.headline)
            Text(status)
                .foregroundStyle(.secondary)
            TextField("код привязки", text: $code)
                .textFieldStyle(.roundedBorder)
            HStack {
                Button("ПОДКЛЮЧИТЬ") { connect() }
                    .disabled(busy || code.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                Button("ЗАКРЫТЬ") { showBind = false }
                    .disabled(busy)
            }
        }
        .padding(24)
        .frame(minWidth: 360)
    }

    private func connect() {
        busy = true
        status = "привязка"
        let claim = code.trimmingCharacters(in: .whitespacesAndNewlines)
        Task.detached {
            let json = redeem(claim)
            await MainActor.run {
                guard let json else {
                    status = "код не принят"
                    busy = false
                    return
                }
                do {
                    try DeviceStore.save(json)
                    bound = true
                    showBind = false
                    status = "подключение"
                    startTunnel()
                } catch {
                    status = "не удалось записать устройство"
                    busy = false
                }
            }
        }
    }

    private func startTunnel() {
        NETunnelProviderManager.loadAllFromPreferences { managers, error in
            if error != nil {
                Task { @MainActor in
                    status = "туннель недоступен"
                    busy = false
                }
                return
            }
            let manager = managers?.first ?? NETunnelProviderManager()
            let proto = NETunnelProviderProtocol()
            proto.providerBundleIdentifier = tunnelBundle
            proto.serverAddress = "m1r4g3.bid"
            manager.protocolConfiguration = proto
            manager.localizedDescription = "m1r4g3"
            manager.isEnabled = true
            manager.saveToPreferences { error in
                if error != nil {
                    Task { @MainActor in
                        status = "туннель не записан"
                        busy = false
                    }
                    return
                }
                manager.loadFromPreferences { error in
                    if error != nil {
                        Task { @MainActor in
                            status = "туннель не прочитан"
                            busy = false
                        }
                        return
                    }
                    do {
                        try manager.connection.startVPNTunnel()
                        Task { @MainActor in
                            status = "подключение"
                            busy = false
                            refreshStatus()
                        }
                    } catch {
                        Task { @MainActor in
                            status = "туннель не стартовал"
                            busy = false
                        }
                    }
                }
            }
        }
    }

    private func unlink() {
        busy = true
        NETunnelProviderManager.loadAllFromPreferences { managers, _ in
            managers?.forEach { $0.connection.stopVPNTunnel() }
            DeviceStore.delete()
            Task { @MainActor in
                bound = false
                code = ""
                status = "не подключено"
                busy = false
            }
        }
    }

    private func refreshStatus() {
        let isBound = bound
        NETunnelProviderManager.loadAllFromPreferences { managers, _ in
            let text: String
            switch managers?.first?.connection.status {
            case .connected:
                text = "подключено"
            case .connecting, .reasserting:
                text = "подключение"
            case .disconnecting:
                text = "отключение"
            default:
                text = isBound ? "привязано" : "не подключено"
            }
            Task { @MainActor in status = text }
        }
    }

    private func openCabinet() {
        #if os(iOS)
        UIApplication.shared.open(cabinetURL)
        #else
        NSWorkspace.shared.open(cabinetURL)
        #endif
    }
}

private func redeem(_ code: String) -> String? {
    guard let cString = code.cString(using: .utf8) else { return nil }
    guard let ptr = mirage_claim_redeem(cString) else { return nil }
    defer { mirage_store_free(ptr) }
    return String(cString: ptr)
}
