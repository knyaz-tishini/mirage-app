import Darwin
import Foundation
import MirageTun
import NetworkExtension

func mirageWritePackets(
    _ data: UnsafePointer<UInt8>?,
    _ len: Int,
    _ ctx: UnsafeMutableRawPointer?
) {
    guard let data, len > 0, let ctx else { return }
    let provider = Unmanaged<PacketTunnelProvider>.fromOpaque(ctx).takeUnretainedValue()
    let raw = Data(bytes: data, count: len)
    let family: Int32 = raw.first.map { ($0 >> 4) == 6 ? AF_INET6 : AF_INET } ?? AF_INET
    provider.packetFlow.writePackets([raw], withProtocols: [NSNumber(value: family)])
}

final class PacketTunnelProvider: NEPacketTunnelProvider {
    private var tun: UnsafeMutableRawPointer?
    private var retained: Unmanaged<PacketTunnelProvider>?

    override func startTunnel(
        options: [String: NSObject]?,
        completionHandler: @escaping (Error?) -> Void
    ) {
        let _ = options
        guard let json = DeviceStore.load() else {
            completionHandler(tunnelError("устройство не привязано"))
            return
        }
        let settings = tunnelSettings()
        setTunnelNetworkSettings(settings) { [weak self] error in
            guard let self else {
                completionHandler(tunnelError("туннель закрыт"))
                return
            }
            if let error {
                completionHandler(error)
                return
            }
            json.withUnsafeBytes { buf in
                guard let base = buf.baseAddress?.assumingMemoryBound(to: UInt8.self) else {
                    completionHandler(tunnelError("пустой файл устройства"))
                    return
                }
                let retained = Unmanaged.passRetained(self)
                let started = mirage_tun_start_artefact(
                    base,
                    buf.count,
                    mirageWritePackets,
                    retained.toOpaque()
                )
                if started == nil {
                    retained.release()
                    completionHandler(tunnelError("сессия не поднялась"))
                    return
                }
                self.retained = retained
                self.tun = started
                self.pumpPackets()
                completionHandler(nil)
            }
        }
    }

    override func stopTunnel(
        with reason: NEProviderStopReason,
        completionHandler: @escaping () -> Void
    ) {
        let _ = reason
        if let tun {
            mirage_tun_stop(tun)
            self.tun = nil
        }
        retained?.release()
        retained = nil
        completionHandler()
    }

    private func pumpPackets() {
        packetFlow.readPackets { [weak self] packets, _ in
            guard let self, let tun = self.tun else { return }
            for packet in packets {
                packet.withUnsafeBytes { buf in
                    guard let base = buf.baseAddress?.assumingMemoryBound(to: UInt8.self) else {
                        return
                    }
                    mirage_tun_process_packet(tun, base, buf.count)
                }
            }
            self.pumpPackets()
        }
    }

    private func tunnelSettings() -> NEPacketTunnelNetworkSettings {
        let settings = NEPacketTunnelNetworkSettings(tunnelRemoteAddress: "10.0.85.1")
        let ipv4 = NEIPv4Settings(addresses: ["10.0.85.1"], subnetMasks: ["255.255.255.0"])
        ipv4.includedRoutes = [NEIPv4Route.default()]
        settings.ipv4Settings = ipv4
        let ipv6 = NEIPv6Settings(addresses: ["fd00:7::2"], networkPrefixLengths: [NSNumber(value: 64)])
        ipv6.includedRoutes = [NEIPv6Route.default()]
        settings.ipv6Settings = ipv6
        settings.mtu = NSNumber(value: 1200)
        return settings
    }
}

private func tunnelError(_ text: String) -> NSError {
    NSError(domain: "bid.m1r4g3.tunnel", code: 1, userInfo: [NSLocalizedDescriptionKey: text])
}
