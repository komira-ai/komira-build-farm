// The fixture app, one source for macOS and iOS: one window holding ContentView.

import SwiftUI

@main
struct FixtureApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}
