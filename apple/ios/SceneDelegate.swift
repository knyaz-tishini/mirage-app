import UIKit

/// iOS 27 SDK refuses to launch an app that has no scene.
/// The oasis window is created by the Rust host and attached to this scene.
final class SceneDelegate: UIResponder, UIWindowSceneDelegate {
    var window: UIWindow?

    func scene(
        _ scene: UIScene,
        willConnectTo session: UISceneSession,
        options connectionOptions: UIScene.ConnectionOptions
    ) {
        _ = session
        _ = connectionOptions
        guard let windowScene = scene as? UIWindowScene else { return }
        adoptLooseWindows(windowScene)
    }

    func sceneDidBecomeActive(_ scene: UIScene) {
        guard let windowScene = scene as? UIWindowScene else { return }
        adoptLooseWindows(windowScene)
    }

    /// winit may build its UIWindow with initWithFrame: before the scene
    /// exists. iOS 27 only shows a window that belongs to the scene.
    private func adoptLooseWindows(_ windowScene: UIWindowScene) {
        let loose = UIApplication.shared.windows.filter { $0.windowScene == nil }
        for window in loose {
            window.windowScene = windowScene
            window.frame = windowScene.coordinateSpace.bounds
            window.makeKeyAndVisible()
            if self.window == nil {
                self.window = window
            }
        }
    }
}
