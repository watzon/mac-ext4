// Regenerate the flat layer artwork of macos/Ext4/AppIcon.icon:
//   swift tools/icon/layers.swift macos/Ext4/AppIcon.icon/Assets
// Colors, Liquid Glass, and per-appearance choices live in AppIcon.icon/icon.json.
import AppKit
import CoreGraphics
import CoreText

// Flat white layer artwork on a 1024 x 1024 canvas (top-left origin math,
// converted to CoreGraphics bottom-left when drawing). Icon Composer
// applies color, glass, highlights, and shadows.
let size = 1024
let out = CommandLine.arguments[1]

func layer(_ name: String, _ draw: (CGContext) -> Void) {
    let c = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: size * 4,
                      space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    c.translateBy(x: 0, y: CGFloat(size)); c.scaleBy(x: 1, y: -1)   // top-left origin
    c.setFillColor(CGColor(gray: 1, alpha: 1))
    draw(c)
    let rep = NSBitmapImageRep(cgImage: c.makeImage()!)
    try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: "\(out)/\(name)"))
}

// Drive body: a portable SSD seen from the front.
let body = CGRect(x: 132, y: 252, width: 760, height: 520)
layer("1-drive.png") { c in
    c.addPath(CGPath(roundedRect: body, cornerWidth: 128, cornerHeight: 128, transform: nil))
    c.fillPath()
}

// "ext4" in SF Pro Rounded Heavy, centered on the drive face. The dark
// variant is white because the drive turns blue in Dark appearance.
func label(_ name: String, _ color: NSColor) {
layer(name) { c in
    let font = CTFontCreateWithName("SFProRounded-Heavy" as CFString, 250, nil)
    let base = NSFont(name: "SFProRounded-Heavy", size: 250) ?? NSFont.systemFont(ofSize: 250, weight: .heavy)
    let rounded = base.fontDescriptor.withDesign(.rounded).flatMap { NSFont(descriptor: $0, size: 250) } ?? base
    _ = font
    let attr = NSAttributedString(string: "ext4", attributes: [.font: rounded, .foregroundColor: color, .kern: -4])
    let line = CTLineCreateWithAttributedString(attr)
    let bounds = CTLineGetImageBounds(line, c)
    c.saveGState()
    c.scaleBy(x: 1, y: -1)   // text draws upright in a bottom-left system
    let x = body.midX - bounds.width / 2 - bounds.minX
    let y = -(body.midY - 18) - bounds.height / 2 - bounds.minY
    c.textPosition = CGPoint(x: x, y: y)
    CTLineDraw(line, c)
    c.restoreGState()
}
}
label("2-label.png", NSColor(srgbRed: 0.05, green: 0.20, blue: 0.48, alpha: 1))
label("2-label-dark.png", NSColor(srgbRed: 0.93, green: 0.96, blue: 1.0, alpha: 1))

// Activity light near the lower-right corner of the face.
layer("3-light.png") { c in
    c.setFillColor(CGColor(srgbRed: 1.0, green: 0.70, blue: 0.18, alpha: 1))
    c.fillEllipse(in: CGRect(x: body.maxX - 132, y: body.maxY - 104, width: 52, height: 52))
}
