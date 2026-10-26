#if os(iOS)
import SwiftUI

@main
struct MirageApp: App {
    var body: some Scene {
        WindowGroup {
            MirageClientView()
                .ignoresSafeArea()
        }
    }
}
#endif
