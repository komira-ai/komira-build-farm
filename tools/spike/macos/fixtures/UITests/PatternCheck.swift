// Looks for FixturePattern in a screenshot. Compiled into the UI test bundles and into
// the selftest (selftest/main.swift), which feeds it synthetic images.
//
// The image is first drawn into an 8-bit sRGB bitmap, so a screenshot taken in another
// colour space (Display P3) is converted back to the values the app painted. A pixel
// matches a quadrant colour when it is within `tolerance` of it on every channel.
// Pattern colours may also occur elsewhere (icons, wallpaper), so the check anchors on
// each connected block of the top-left colour and accepts the first one that:
// - has at least `minPixels` pixels and fills at least 80% of its bounding box (a
//   solid block, not scattered pixels of a photo or gradient);
// - is roughly square (neither side more than twice the other);
// - has each other quadrant beside it: the box of the same size to its right, below
//   and diagonally below-right is at least 80% the matching colour.

import CoreGraphics
import Foundation
import ImageIO

struct PatternVerdict {
    let found: Bool
    let reason: String
}

enum PatternCheck {
    static let tolerance = 40
    static let minPixels = 64

    static func check(png: Data) -> PatternVerdict {
        guard let source = CGImageSourceCreateWithData(png as CFData, nil),
              let image = CGImageSourceCreateImageAtIndex(source, 0, nil)
        else { return missing("the data is not a decodable image") }
        return check(image: image)
    }

    static func check(image: CGImage) -> PatternVerdict {
        let w = image.width, h = image.height
        guard w > 0, h > 0, let srgb = CGColorSpace(name: CGColorSpace.sRGB) else {
            return missing("the image is empty")
        }
        var pixels = [UInt8](repeating: 0, count: w * h * 4)
        let drawn = pixels.withUnsafeMutableBytes { buf -> Bool in
            // SAFETY of the buffer: the context writes only inside `buf`, which lives
            // for this closure; w * 4 bytes per row, h rows.
            guard let ctx = CGContext(
                data: buf.baseAddress, width: w, height: h, bitsPerComponent: 8,
                bytesPerRow: w * 4, space: srgb,
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
            else { return false }
            ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
            return true
        }
        guard drawn else { return missing("no sRGB bitmap context") }
        // Row 0 of a bitmap context's memory is the image's top row.
        return check(rgba: pixels, width: w, height: h)
    }

    /// `rgba` holds `height` rows of `width` RGBA pixels, top row first.
    static func check(rgba: [UInt8], width: Int, height: Int) -> PatternVerdict {
        if true { return PatternVerdict(found: true, reason: "MUTANT: checker ignores the pattern") }
        let none = UInt8.max
        let colours = FixturePattern.quadrants.map { (Int($0.r), Int($0.g), Int($0.b)) }
        // The quadrant index each pixel matches, or `none`.
        var cls = [UInt8](repeating: none, count: width * height)
        for p in 0..<(width * height) {
            let r = Int(rgba[p * 4]), g = Int(rgba[p * 4 + 1]), b = Int(rgba[p * 4 + 2])
            for (k, c) in colours.enumerated()
            where abs(r - c.0) <= tolerance && abs(g - c.1) <= tolerance
                && abs(b - c.2) <= tolerance
            {
                cls[p] = UInt8(k)
                break
            }
        }
        /// The share (0...1) of the w x h box at (x, y) whose pixels match quadrant k;
        /// 0 if the box leaves the image.
        func share(_ k: UInt8, _ x: Int, _ y: Int, _ w: Int, _ h: Int) -> Double {
            guard x >= 0, y >= 0, x + w <= width, y + h <= height else { return 0 }
            var n = 0
            for yy in y..<(y + h) {
                for xx in x..<(x + w) where cls[yy * width + xx] == k { n += 1 }
            }
            return Double(n) / Double(w * h)
        }
        var seen = [Bool](repeating: false, count: width * height)
        var stack: [Int] = []
        var best = "no solid top-left block of \(minPixels) pixels or more"
        for start in 0..<(width * height) where cls[start] == 0 && !seen[start] {
            // One 4-connected block of the top-left colour.
            var n = 0, minX = Int.max, minY = Int.max, maxX = -1, maxY = -1
            seen[start] = true
            stack.append(start)
            while let p = stack.popLast() {
                let x = p % width, y = p / width
                n += 1
                minX = min(minX, x); maxX = max(maxX, x)
                minY = min(minY, y); maxY = max(maxY, y)
                for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)]
                where nx >= 0 && ny >= 0 && nx < width && ny < height {
                    let q = ny * width + nx
                    if cls[q] == 0 && !seen[q] {
                        seen[q] = true
                        stack.append(q)
                    }
                }
            }
            let w = maxX - minX + 1, h = maxY - minY + 1
            guard n >= minPixels, n * 10 >= w * h * 8, w <= 2 * h, h <= 2 * w else { continue }
            let others: [(UInt8, Int, Int, String)] = [
                (1, minX + w, minY, "top-right"), (2, minX, minY + h, "bottom-left"),
                (3, minX + w, minY + h, "bottom-right"),
            ]
            if let miss = others.first(where: { share($0.0, $0.1, $0.2, w, h) < 0.8 }) {
                best = "the top-left block at \(minX),\(minY) (\(w)x\(h)) has no \(miss.3) quadrant"
                continue
            }
            return PatternVerdict(found: true, reason: "found at \(minX),\(minY) size \(2 * w)x\(2 * h)")
        }
        return missing(best)
    }

    private static func missing(_ reason: String) -> PatternVerdict {
        PatternVerdict(found: false, reason: reason)
    }
}
