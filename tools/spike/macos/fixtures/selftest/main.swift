// The pixel checker's selftest: PatternCheck against synthetic images, each one PNG
// encoded and decoded the way a screenshot attachment is. Exits 1 if any case gets the
// wrong verdict. Built and run by selftest.sh (macOS only: CoreGraphics and ImageIO).

import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let W = 600, H = 800

/// An sRGB image of `W` x `H`, filled with `background`, then each (rect, colour) in
/// top-down coordinates (y = 0 is the top row).
func image(background: (UInt8, UInt8, UInt8), _ fills: [(CGRect, (UInt8, UInt8, UInt8))])
    -> CGImage
{
    let ctx = CGContext(
        data: nil, width: W, height: H, bitsPerComponent: 8, bytesPerRow: W * 4,
        space: CGColorSpace(name: CGColorSpace.sRGB)!,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    func fill(_ r: CGRect, _ c: (UInt8, UInt8, UInt8)) {
        ctx.setFillColor(red: CGFloat(c.0) / 255, green: CGFloat(c.1) / 255,
                         blue: CGFloat(c.2) / 255, alpha: 1)
        ctx.fill(CGRect(x: r.minX, y: CGFloat(H) - r.maxY, width: r.width, height: r.height))
    }
    fill(CGRect(x: 0, y: 0, width: W, height: H), background)
    for (r, c) in fills { fill(r, c) }
    return ctx.makeImage()!
}

/// The pattern's four quadrants at (x, y) with side `side`, colours in `order`.
func quadrants(x: CGFloat, y: CGFloat, side: CGFloat, order: [Int] = [0, 1, 2, 3])
    -> [(CGRect, (UInt8, UInt8, UInt8))]
{
    let h = side / 2
    let at = [CGPoint(x: x, y: y), CGPoint(x: x + h, y: y),
              CGPoint(x: x, y: y + h), CGPoint(x: x + h, y: y + h)]
    return order.enumerated().map { slot, k in
        let q = FixturePattern.quadrants[k]
        return (CGRect(x: at[slot].x, y: at[slot].y, width: h, height: h), (q.r, q.g, q.b))
    }
}

/// A wallpaper stand-in: vertical bands sweeping through the hues, so every pattern
/// colour occurs, but not as the pattern.
func wallpaper() -> CGImage {
    var fills: [(CGRect, (UInt8, UInt8, UInt8))] = []
    let hues: [(UInt8, UInt8, UInt8)] = [
        (255, 0, 0), (255, 128, 0), (255, 255, 0), (128, 255, 0), (0, 255, 0),
        (0, 255, 255), (0, 128, 255), (0, 0, 255), (128, 0, 255), (255, 0, 255),
    ]
    let band = CGFloat(W) / CGFloat(hues.count)
    for (i, c) in hues.enumerated() {
        fills.append((CGRect(x: CGFloat(i) * band, y: 0, width: band, height: CGFloat(H)), c))
    }
    return image(background: (0, 0, 0), fills)
}

func png(_ image: CGImage) -> Data {
    let data = NSMutableData()
    let dest = CGImageDestinationCreateWithData(data, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(dest, image, nil)
    precondition(CGImageDestinationFinalize(dest))
    return data as Data
}

let grey: (UInt8, UInt8, UInt8) = (236, 236, 236)
let cases: [(String, CGImage, Bool)] = [
    ("pattern on a light window", image(background: grey, quadrants(x: 200, y: 120, side: 320)), true),
    ("small pattern on a dark window", image(background: (30, 30, 30), quadrants(x: 20, y: 600, side: 40)), true),
    // A screen: the pattern in a window, plus pattern colours elsewhere (icons, a
    // wallpaper strip, a bigger red block). The hosted macOS screen screenshot looked
    // like this, and a checker reading each colour's pixels as one block missed it.
    ("pattern among other pattern-coloured pixels", image(background: grey, quadrants(x: 200, y: 120, side: 320) + [
        (CGRect(x: 10, y: 10, width: 6, height: 6), (255, 0, 0)),
        (CGRect(x: 560, y: 760, width: 30, height: 30), (255, 255, 0)),
        (CGRect(x: 0, y: 780, width: 600, height: 4), (0, 0, 255)),
        (CGRect(x: 20, y: 500, width: 170, height: 170), (255, 0, 0)),
    ]), true),
    ("all black", image(background: (0, 0, 0), []), false),
    ("wallpaper only", wallpaper(), false),
    ("quadrants in the wrong order", image(background: grey, quadrants(x: 200, y: 120, side: 320, order: [3, 1, 2, 0])), false),
    ("three quadrants", image(background: grey, Array(quadrants(x: 200, y: 120, side: 320).prefix(3))), false),
    ("quadrants scattered apart", image(background: grey, quadrants(x: 0, y: 0, side: 200).enumerated().map { i, f in
        (f.0.offsetBy(dx: CGFloat(i % 2) * 300, dy: CGFloat(i / 2) * 500), f.1) }), false),
]

var failures = 0
for (name, img, want) in cases {
    let verdict = PatternCheck.check(png: png(img))
    let ok = verdict.found == want
    if !ok { failures += 1 }
    print("\(ok ? "ok  " : "FAIL") \(name): want \(want ? "found" : "missing"), "
          + "got \(verdict.found ? "found" : "missing") (\(verdict.reason))")
}
print("selftest: \(cases.count - failures) of \(cases.count) cases right")
exit(failures == 0 ? 0 : 1)
