// The fixture pattern: a square split into four quadrants of known sRGB colours.
// The app paints it (App/ContentView.swift); UITests/PatternCheck.swift looks for it in
// a screenshot. Both read the colours from here, so they cannot disagree.

enum FixturePattern {
    /// The quadrant colours in 8-bit sRGB, in the order top-left, top-right,
    /// bottom-left, bottom-right.
    static let quadrants: [(r: UInt8, g: UInt8, b: UInt8)] = [
        (255, 0, 0),
        (0, 255, 0),
        (0, 0, 255),
        (255, 255, 0),
    ]

    /// The side of the painted square, in points.
    static let side: Double = 160

    /// A launch argument that makes the app leave the pattern out: the control arm of
    /// the screenshot test, which must then find no pattern.
    static let hideArgument = "-kbf-no-pattern"
}
