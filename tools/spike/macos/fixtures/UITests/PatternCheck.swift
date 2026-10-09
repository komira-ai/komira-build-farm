// Looks for FixturePattern in a screenshot. Compiled into the UI test bundles and into
// the selftest (selftest/main.swift), which feeds it synthetic images.
//
// The image is first drawn into an 8-bit sRGB bitmap, so a screenshot taken in another
// colour space (Display P3) is converted back to the values the app painted. Then, for
// each quadrant colour, the pixels within `tolerance` of it on every channel are
// counted. The pattern is found only when:
// - every colour has at least `minPixels` matching pixels;
// - each colour's pixels fill at least 80% of their own bounding box (a solid block,
//   not scattered pixels of a photo or gradient);
// - the colours' centres sit in the pattern's order (top-left left of top-right and
//   above bottom-left, and so on);
// - the four boxes together fill at least 80% of their union, which is roughly square.

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
        struct Box {
            var n = 0, sumX = 0, sumY = 0
            var minX = Int.max, minY = Int.max, maxX = -1, maxY = -1
            var area: Int { n == 0 ? 0 : (maxX - minX + 1) * (maxY - minY + 1) }
        }
        let colours = FixturePattern.quadrants.map { (Int($0.r), Int($0.g), Int($0.b)) }
        var boxes = [Box](repeating: Box(), count: colours.count)
        for y in 0..<height {
            for x in 0..<width {
                let i = (y * width + x) * 4
                let r = Int(rgba[i]), g = Int(rgba[i + 1]), b = Int(rgba[i + 2])
                for (k, c) in colours.enumerated()
                where abs(r - c.0) <= tolerance && abs(g - c.1) <= tolerance
                    && abs(b - c.2) <= tolerance
                {
                    boxes[k].n += 1
                    boxes[k].sumX += x
                    boxes[k].sumY += y
                    boxes[k].minX = min(boxes[k].minX, x)
                    boxes[k].maxX = max(boxes[k].maxX, x)
                    boxes[k].minY = min(boxes[k].minY, y)
                    boxes[k].maxY = max(boxes[k].maxY, y)
                    break
                }
            }
        }
        for (k, b) in boxes.enumerated() {
            if b.n < minPixels {
                return missing("quadrant \(k) has \(b.n) matching pixels (< \(minPixels))")
            }
            if b.n * 10 < b.area * 8 {
                return missing("quadrant \(k) fills \(b.n) of its \(b.area)-pixel box (< 80%)")
            }
        }
        let cx = boxes.map { Double($0.sumX) / Double($0.n) }
        let cy = boxes.map { Double($0.sumY) / Double($0.n) }
        guard cx[0] < cx[1], cx[2] < cx[3], cy[0] < cy[2], cy[1] < cy[3] else {
            return missing("the quadrants are out of order")
        }
        let minX = boxes.map(\.minX).min()!, maxX = boxes.map(\.maxX).max()!
        let minY = boxes.map(\.minY).min()!, maxY = boxes.map(\.maxY).max()!
        let uw = maxX - minX + 1, uh = maxY - minY + 1
        let total = boxes.reduce(0) { $0 + $1.n }
        guard total * 10 >= uw * uh * 8 else {
            return missing("the quadrants fill \(total) of their \(uw * uh)-pixel union (< 80%)")
        }
        guard uw * 2 >= uh, uh * 2 >= uw else {
            return missing("the union is \(uw)x\(uh), not roughly square")
        }
        return PatternVerdict(found: true, reason: "found at \(minX),\(minY) size \(uw)x\(uh)")
    }

    private static func missing(_ reason: String) -> PatternVerdict {
        PatternVerdict(found: false, reason: reason)
    }
}
