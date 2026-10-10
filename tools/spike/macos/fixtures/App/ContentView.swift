// The fixture's only view: the colour pattern, a label and a button. The label reads
// "ready" until the button is tapped, then "tapped <n>". The UI tests find each
// element by its accessibility identifier ("pattern", "label", "button").

import Foundation
import SwiftUI

struct ContentView: View {
    @State private var taps = 0
    private let showPattern = !ProcessInfo.processInfo.arguments.contains(
        FixturePattern.hideArgument)

    var body: some View {
        VStack(spacing: 24) {
            if showPattern {
                PatternView()
                    .frame(width: FixturePattern.side, height: FixturePattern.side)
                    .accessibilityIdentifier("pattern")
            }
            Text(taps == 0 ? "ready" : "tapped \(taps)")
                .accessibilityIdentifier("label")
            Button("Tap") { taps += 1 }
                .accessibilityIdentifier("button")
        }
        .padding(32)
        .frame(minWidth: 320, minHeight: 360)
    }
}

/// The four quadrants of FixturePattern, edge to edge.
struct PatternView: View {
    var body: some View {
        let q = FixturePattern.quadrants.map { c in
            Color(.sRGB, red: Double(c.r) / 255, green: Double(c.g) / 255,
                  blue: Double(c.b) / 255, opacity: 1)
        }
        VStack(spacing: 0) {
            HStack(spacing: 0) { q[0]; q[1] }
            HStack(spacing: 0) { q[2]; q[3] }
        }
    }
}
