// Draws the menu-bar icon: the Plow mark (Resources/MenuMark*.png) with a
// solid sync dot at the bottom right, cut out of the mark with a clear ring
// so it stays readable at 18 pt. The result stays one color on transparency,
// so macOS can still tint it as a template image.
//
//   swift menu-icon.swift      writes Resources/MenuIcon.png and MenuIcon@2x.png
import AppKit

let resources = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("Resources")

for (suffix, scale) in [("", 1.0), ("@2x", 2.0)] {
    let source = resources.appendingPathComponent("MenuMark\(suffix).png")
    guard let mark = NSImage(contentsOf: source)?.cgImage(forProposedRect: nil, context: nil, hints: nil)
    else { fatalError("can't read \(source.path)") }
    let size = CGFloat(mark.width)
    let context = CGContext(data: nil, width: mark.width, height: mark.height, bitsPerComponent: 8,
                            bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    context.draw(mark, in: CGRect(x: 0, y: 0, width: size, height: size))
    // In points: a 5 pt dot touching the corner, with a 1 pt gap around it.
    let radius = 2.5 * scale, gap = 1 * scale
    let center = CGPoint(x: size - radius, y: radius)
    context.setBlendMode(.clear)
    context.fillEllipse(in: CGRect(x: center.x - radius - gap, y: center.y - radius - gap,
                                   width: 2 * (radius + gap), height: 2 * (radius + gap)))
    context.setBlendMode(.normal)
    context.setFillColor(.black)
    context.fillEllipse(in: CGRect(x: center.x - radius, y: center.y - radius, width: 2 * radius, height: 2 * radius))
    let png = NSBitmapImageRep(cgImage: context.makeImage()!).representation(using: .png, properties: [:])!
    try png.write(to: resources.appendingPathComponent("MenuIcon\(suffix).png"))
}
