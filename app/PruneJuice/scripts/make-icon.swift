// The Prune Juice icon: the half-full droplet, in the app's plum.
//
// The same mark the sidebar shows, at icon size. The colour is `plum` from
// Views.swift — Color(red: 0.55, green: 0.39, blue: 0.78) — so the icon in
// the Dock and the mark in the window are the same purple, not two purples
// that were each picked to look right on their own.
//
// The droplet is drawn here rather than taken from SF Symbols on purpose:
// Apple's licence for those symbols does not cover app icons. This is the
// same shape, owned.
//
// Drawn in code rather than checked in as a binary, for the same reason the
// appcast is generated rather than hand-edited: the shape is reviewable in a
// diff, every size is rendered at its own resolution instead of being
// downsampled from one master, and changing the palette is a one-line change
// with no design tool in the loop.
//
//   swift make-icon.swift <out.iconset>
//   iconutil -c icns <out.iconset>

import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

func rgb(_ r: Int, _ g: Int, _ b: Int, _ a: CGFloat = 1) -> CGColor {
    CGColor(red: CGFloat(r) / 255, green: CGFloat(g) / 255, blue: CGFloat(b) / 255, alpha: a)
}

/// `plum`, and two shades of it. The middle one is the app's exact value; the
/// others only lighten and deepen it, so there is still one colour here.
let plumLight = rgb(163, 126, 214)
let plum      = rgb(140, 99, 199)
let plumDeep  = rgb(104, 66, 163)

let bgTop     = rgb(247, 242, 255)
let bgBottom  = rgb(204, 184, 237)

/// Apple's icon outline: an 824-wide body on a 1024 canvas, corner radius
/// 185.4. The margin is not wasted space — it is where the shadow the system
/// draws around an icon goes.
func squircle(_ r: CGRect) -> CGPath {
    let radius = r.width * 185.4 / 824.0
    return CGPath(roundedRect: r, cornerWidth: radius, cornerHeight: radius, transform: nil)
}

/// Everything is expressed as a fraction of the body, so the drawing is the
/// same shape at 16 points and at 1024 and nothing is tuned per size.
func draw(_ c: CGContext, _ S: CGFloat) {
    let cs = CGColorSpaceCreateDeviceRGB()
    let inset = S * 100.0 / 1024.0
    let body = CGRect(x: inset, y: inset, width: S - 2 * inset, height: S - 2 * inset)
    let W = body.width
    func px(_ x: CGFloat) -> CGFloat { body.minX + x * W }
    func py(_ y: CGFloat) -> CGFloat { body.minY + y * W }

    c.saveGState()
    c.addPath(squircle(body))
    c.clip()
    let bg = CGGradient(colorsSpace: cs, colors: [bgTop, bgBottom] as CFArray, locations: [0, 1])!
    c.drawLinearGradient(bg, start: CGPoint(x: body.minX, y: body.maxY),
                         end: CGPoint(x: body.maxX, y: body.minY), options: [])

    // A teardrop: an apex, two curves falling away from it, and a circle for
    // the bottom. The lower control point of each curve sits directly above
    // the circle's widest point, which is what makes the curve meet the arc
    // without a crease.
    let apexY: CGFloat = 0.905
    let cy: CGFloat = 0.400
    let R: CGFloat = 0.255
    let level: CGFloat = 0.500   // how full it is

    let drop = CGMutablePath()
    drop.move(to: CGPoint(x: px(0.5), y: py(apexY)))
    drop.addCurve(to: CGPoint(x: px(0.5 - R), y: py(cy)),
                  control1: CGPoint(x: px(0.5 - 0.058), y: py(0.735)),
                  control2: CGPoint(x: px(0.5 - R), y: py(0.632)))
    // Counterclockwise in a y-up space is π → 3π/2 → 2π, which is the bottom
    // half. Clockwise here would arc over the top and fold the shape inside
    // out.
    drop.addArc(center: CGPoint(x: px(0.5), y: py(cy)), radius: R * W,
                startAngle: .pi, endAngle: 2 * .pi, clockwise: false)
    drop.addCurve(to: CGPoint(x: px(0.5), y: py(apexY)),
                  control1: CGPoint(x: px(0.5 + R), y: py(0.632)),
                  control2: CGPoint(x: px(0.5 + 0.058), y: py(0.735)))
    drop.closeSubpath()

    // Half full, and the fill line is straight, as it is in the symbol. The
    // droplet is a container here, not a falling drop: what is in it is what
    // came back.
    c.saveGState()
    c.setShadow(offset: CGSize(width: 0, height: -0.016 * W), blur: 0.05 * W,
                color: rgb(70, 40, 120, 0.22))
    c.addPath(drop)
    c.clip()
    // Everything below the fill line. The rectangle starts under the droplet
    // so only its top edge does any cutting.
    c.clip(to: CGRect(x: body.minX, y: body.minY, width: W, height: (level - 0.0) * W))
    let juice = CGGradient(colorsSpace: cs, colors: [plum, plumDeep] as CFArray,
                           locations: [0, 1])!
    c.drawLinearGradient(juice, start: CGPoint(x: 0, y: py(level)),
                         end: CGPoint(x: 0, y: py(cy - R)), options: [])
    c.restoreGState()

    // The outline, stroked after the fill so the fill cannot spill past it.
    // Weight roughly matches the symbol at .medium, which is what the sidebar
    // draws.
    c.saveGState()
    c.setShadow(offset: CGSize(width: 0, height: -0.016 * W), blur: 0.05 * W,
                color: rgb(70, 40, 120, 0.22))
    c.setLineWidth(0.030 * W)
    c.setStrokeColor(plum)
    c.addPath(drop)
    c.strokePath()
    c.restoreGState()

    // A single highlight in the empty half. Without it the top reads as a
    // hole rather than as the part that is still to fill.
    c.saveGState()
    c.addPath(drop)
    c.clip()
    let gloss = CGGradient(colorsSpace: cs,
                           colors: [plumLight.copy(alpha: 0.22)!,
                                    plumLight.copy(alpha: 0.0)!] as CFArray,
                           locations: [0, 1])!
    c.drawLinearGradient(gloss, start: CGPoint(x: 0, y: py(apexY)),
                         end: CGPoint(x: 0, y: py(level)), options: [])
    c.restoreGState()

    c.restoreGState()
}

func write(size: Int, to path: String) {
    let cs = CGColorSpaceCreateDeviceRGB()
    guard let ctx = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8,
                              bytesPerRow: 0, space: cs,
                              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else {
        FileHandle.standardError.write(Data("error: no bitmap context at \(size)px\n".utf8))
        exit(1)
    }
    ctx.setAllowsAntialiasing(true)
    ctx.interpolationQuality = .high
    draw(ctx, CGFloat(size))
    guard let img = ctx.makeImage(),
          let dest = CGImageDestinationCreateWithURL(URL(fileURLWithPath: path) as CFURL,
                                                     UTType.png.identifier as CFString, 1, nil)
    else {
        FileHandle.standardError.write(Data("error: cannot write \(path)\n".utf8))
        exit(1)
    }
    CGImageDestinationAddImage(dest, img, nil)
    if !CGImageDestinationFinalize(dest) {
        FileHandle.standardError.write(Data("error: cannot finalise \(path)\n".utf8))
        exit(1)
    }
}

guard CommandLine.arguments.count > 1 else {
    FileHandle.standardError.write(Data("usage: make-icon.swift <out.iconset>\n".utf8))
    exit(2)
}
let out = CommandLine.arguments[1]
try? FileManager.default.createDirectory(atPath: out, withIntermediateDirectories: true)

// The names iconutil expects. A point size appears twice where a 1x and a 2x
// slot land on the same pixel count; both files are required.
let slots: [(Int, String)] = [
    (16, "icon_16x16.png"),
    (32, "icon_16x16@2x.png"),
    (32, "icon_32x32.png"),
    (64, "icon_32x32@2x.png"),
    (128, "icon_128x128.png"),
    (256, "icon_128x128@2x.png"),
    (256, "icon_256x256.png"),
    (512, "icon_256x256@2x.png"),
    (512, "icon_512x512.png"),
    (1024, "icon_512x512@2x.png"),
]
for (size, name) in slots { write(size: size, to: "\(out)/\(name)") }
