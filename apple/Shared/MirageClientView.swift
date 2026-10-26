import NetworkExtension
import SwiftUI
import UIKit

private let tunnelBundle = "bid.m1r4g3.tunnel"

@_silgen_name("mirage_gui_frame")
func mirage_gui_frame(_ width: UInt32, _ height: UInt32, _ dpr: Double, _ pixels: UnsafeMutablePointer<UInt8>?) -> Int32

@_silgen_name("mirage_gui_pointer")
func mirage_gui_pointer(_ phase: Int32, _ x: Double, _ y: Double)

@_silgen_name("mirage_gui_text")
func mirage_gui_text(_ utf8: UnsafePointer<CChar>?)

@_silgen_name("mirage_gui_key")
func mirage_gui_key(_ key: Int32)

@_silgen_name("mirage_gui_set_unlink")
func mirage_gui_set_unlink(_ unlink: (@convention(c) () -> Void)?)

@_silgen_name("mirage_gui_bind_result")
func mirage_gui_bind_result(_ utf8: UnsafePointer<CChar>?)

@_silgen_name("mirage_gui_set_bound")
func mirage_gui_set_bound(_ bound: Int32, _ name: UnsafePointer<CChar>?)

@_silgen_name("mirage_gui_set_hooks")
func mirage_gui_set_hooks(
    _ start: (@convention(c) (UnsafePointer<CChar>?) -> Void)?,
    _ stop: (@convention(c) () -> Void)?,
    _ status: (@convention(c) () -> Int32)?,
    _ bind: (@convention(c) (UnsafePointer<CChar>?) -> Void)?
)

@_silgen_name("mirage_gui_wants_keyboard")
func mirage_gui_wants_keyboard() -> Int32

@_silgen_name("mirage_claim_redeem")
func mirage_claim_redeem(_ code: UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>?

@_silgen_name("mirage_store_free")
func mirage_store_free(_ ptr: UnsafeMutablePointer<CChar>?)

enum TunnelBridge {
    static var phase: Int32 = 0

    static func install() {
        mirage_gui_set_hooks(startTunnel, stopTunnel, tunnelStatus, bindClaim)
        mirage_gui_set_unlink(unlinkDevice)
        if DeviceStore.exists {
            mirage_gui_set_bound(1, nil)
        }
    }

    static let tunnelStatus: @convention(c) () -> Int32 = {
        phase
    }

    static let startTunnel: @convention(c) (UnsafePointer<CChar>?) -> Void = { raw in
        let passed = raw.map { String(cString: $0) }.filter { !$0.isEmpty }
        phase = 1
        NETunnelProviderManager.loadAllFromPreferences { managers, error in
            if error != nil {
                phase = 0
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
                    phase = 0
                    return
                }
                manager.loadFromPreferences { error in
                    if error != nil {
                        phase = 0
                        return
                    }
                    do {
                        if let passed {
                            try manager.connection.startVPNTunnel(options: [
                                "m1r4g3-artefact": passed as NSString
                            ])
                        } else {
                            try manager.connection.startVPNTunnel()
                        }
                    } catch {
                        phase = 0
                    }
                }
            }
        }
        watch(NETunnelProviderManager())
    }

    static let stopTunnel: @convention(c) () -> Void = {
        phase = 0
        NETunnelProviderManager.loadAllFromPreferences { managers, _ in
            managers?.forEach { $0.connection.stopVPNTunnel() }
        }
    }

    static let unlinkDevice: @convention(c) () -> Void = {
        phase = 0
        NETunnelProviderManager.loadAllFromPreferences { managers, _ in
            managers?.forEach { $0.connection.stopVPNTunnel() }
            DeviceStore.delete()
            mirage_gui_set_bound(0, nil)
        }
    }

    static let bindClaim: @convention(c) (UnsafePointer<CChar>?) -> Void = { raw in
        guard let raw else { return }
        let code = String(cString: raw)
        DispatchQueue.global(qos: .userInitiated).async {
            guard let cString = code.cString(using: .utf8) else { return }
            guard let jsonPtr = mirage_claim_redeem(cString) else {
                note("код не принят")
                return
            }
            let json = String(cString: jsonPtr)
            mirage_store_free(jsonPtr)
            DispatchQueue.main.async {
                do {
                    try DeviceStore.save(json)
                    mirage_gui_set_bound(1, nil)
                    note("привязано")
                } catch {
                    note("не удалось записать устройство")
                }
            }
        }
    }

    private static func note(_ text: String) {
        DispatchQueue.main.async {
            text.withCString { mirage_gui_bind_result($0) }
        }
    }

    private static func watch(_ seed: NETunnelProviderManager) {
        NETunnelProviderManager.loadAllFromPreferences { managers, _ in
            let connection = managers?.first?.connection ?? seed.connection
            NotificationCenter.default.addObserver(
                forName: .NEVPNStatusDidChange,
                object: connection,
                queue: .main
            ) { _ in
                switch connection.status {
                case .connected:
                    phase = 2
                case .connecting, .reasserting:
                    phase = 1
                default:
                    phase = 0
                }
            }
        }
    }
}

struct MirageClientView: UIViewControllerRepresentable {
    func makeUIViewController(context: Context) -> MirageClientController {
        MirageClientController()
    }

    func updateUIViewController(_ controller: MirageClientController, context: Context) {}
}

final class MirageClientController: UIViewController, UITextFieldDelegate {
    private let canvas = UIImageView()
    private let field = UITextField()
    private var link: CADisplayLink?
    private var pixels = [UInt8]()
    private var installed = false
    private var keyboardArmed = false

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = UIColor(red: 0x0B / 255, green: 0x0B / 255, blue: 0x0E / 255, alpha: 1)
        canvas.frame = contentRect()
        canvas.contentMode = .scaleToFill
        view.addSubview(canvas)
        field.autocorrectionType = .no
        field.autocapitalizationType = .none
        field.spellCheckingType = .no
        field.delegate = self
        field.frame = CGRect(x: -10, y: -10, width: 1, height: 1)
        field.addTarget(self, action: #selector(textChanged), for: .editingChanged)
        view.addSubview(field)
        let touch = TouchPad(frame: view.bounds)
        touch.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        touch.onTouch = { [weak self] in self?.keyboardArmed = true }
        view.addSubview(touch)
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        canvas.frame = contentRect()
    }

    private func contentRect() -> CGRect {
        view.bounds.inset(by: view.safeAreaInsets)
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        if !installed {
            TunnelBridge.install()
            installed = true
        }
        let link = CADisplayLink(target: self, selector: #selector(tick))
        link.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: 60, preferred: 60)
        link.add(to: .main, forMode: .common)
        self.link = link
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        link?.invalidate()
        link = nil
    }

    @objc private func tick() {
        let scale = view.window?.screen.scale ?? view.contentScaleFactor
        let width = max(1, Int((canvas.bounds.width * scale).rounded()))
        let height = max(1, Int((canvas.bounds.height * scale).rounded()))
        let count = width * height * 4
        if pixels.count != count {
            pixels = [UInt8](repeating: 0, count: count)
        }
        let ok = pixels.withUnsafeMutableBufferPointer { buf -> Int32 in
            guard let base = buf.baseAddress else { return 1 }
            return mirage_gui_frame(UInt32(width), UInt32(height), Double(scale), base)
        }
        if ok == 0, let image = image(width: width, height: height, scale: scale) {
            canvas.image = image
        }
        let want = keyboardArmed && mirage_gui_wants_keyboard() == 1
        if want && !field.isFirstResponder {
            field.becomeFirstResponder()
        } else if !want && field.isFirstResponder {
            field.resignFirstResponder()
            field.text = ""
        }
    }

    private func image(width: Int, height: Int, scale: CGFloat) -> UIImage? {
        let data = Data(pixels)
        guard let provider = CGDataProvider(data: data as CFData) else { return nil }
        guard let cg = CGImage(
            width: width,
            height: height,
            bitsPerComponent: 8,
            bitsPerPixel: 32,
            bytesPerRow: width * 4,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue | CGBitmapInfo.byteOrder32Big.rawValue),
            provider: provider,
            decode: nil,
            shouldInterpolate: false,
            intent: .defaultIntent
        ) else { return nil }
        return UIImage(cgImage: cg, scale: scale, orientation: .up)
    }

    @objc private func textChanged() {
        guard let text = field.text, !text.isEmpty else { return }
        text.withCString { mirage_gui_text($0) }
        field.text = ""
    }

    func textField(_ textField: UITextField, shouldChangeCharactersIn range: NSRange, replacementString string: String) -> Bool {
        if string.isEmpty {
            mirage_gui_key(1)
            return false
        }
        return true
    }

    func textFieldShouldReturn(_ textField: UITextField) -> Bool {
        mirage_gui_key(2)
        return false
    }
}

private final class TouchPad: UIView {
    var onTouch: (() -> Void)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = false
        backgroundColor = .clear
    }

    required init?(coder: NSCoder) { nil }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        onTouch?()
        send(0, touches)
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        send(1, touches)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        send(2, touches)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        send(3, touches)
    }

    private func send(_ phase: Int32, _ touches: Set<UITouch>) {
        guard let point = touches.first?.location(in: self) else { return }
        let inset = safeAreaInsets
        mirage_gui_pointer(phase, Double(point.x - inset.left), Double(point.y - inset.top))
    }
}
