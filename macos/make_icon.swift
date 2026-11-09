import AppKit

// Generates a pixel-perfect macOS Big Sur/Monterey/Ventura/Sonoma/Sequoia HIG compliant app icon.
// Apple HIG specs: 1024x1024 canvas, 824x824 squircle body, 100px margin, 185.4px corner radius,
// double-layer drop shadow, 1px inner rim highlight.

let args = CommandLine.arguments
let scriptDir = URL(fileURLWithPath: CommandLine.arguments[0]).deletingLastPathComponent().path
let rootDir = (scriptDir as NSString).appendingPathComponent("..")

let outputPath = args.count > 1 ? args[1] : (rootDir as NSString).appendingPathComponent("assets/icon/macos.png")
let markPath = (rootDir as NSString).appendingPathComponent("assets/icon/mark.png")

let canvasSize: CGFloat = 1024
let bodySize: CGFloat = 824
let margin: CGFloat = (canvasSize - bodySize) / 2 // 100
let cornerRadius: CGFloat = 185.4

let colorSpace = CGColorSpace(name: CGColorSpace.sRGB)!
guard let ctx = CGContext(
    data: nil,
    width: Int(canvasSize),
    height: Int(canvasSize),
    bitsPerComponent: 8,
    bytesPerRow: 0,
    space: colorSpace,
    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
) else {
    fputs("Failed to create CGContext\n", stderr)
    exit(1)
}

let rect = CGRect(x: margin, y: margin, width: bodySize, height: bodySize)
let squirclePath = CGPath(roundedRect: rect, cornerWidth: cornerRadius, cornerHeight: cornerRadius, transform: nil)

// 1. Double Drop Shadow (Key + Ambient)
// Key shadow: directional depth downwards
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -14), blur: 24, color: CGColor(red: 0, green: 0, blue: 0, alpha: 0.28))
ctx.addPath(squirclePath)
ctx.setFillColor(CGColor(red: 0, green: 0, blue: 0, alpha: 1.0))
ctx.fillPath()
ctx.restoreGState()

// Ambient shadow: soft spread
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -4), blur: 8, color: CGColor(red: 0, green: 0, blue: 0, alpha: 0.16))
ctx.addPath(squirclePath)
ctx.setFillColor(CGColor(red: 0, green: 0, blue: 0, alpha: 1.0))
ctx.fillPath()
ctx.restoreGState()

// 2. Draw Squircle Body with Gradient
ctx.saveGState()
ctx.addPath(squirclePath)
ctx.clip()

// Gradient from Bottom (#FA243C) to Top (#FB5B73)
let gradientColors = [
    CGColor(red: 0xFA / 255.0, green: 0x24 / 255.0, blue: 0x3C / 255.0, alpha: 1.0),
    CGColor(red: 0xFB / 255.0, green: 0x5B / 255.0, blue: 0x73 / 255.0, alpha: 1.0)
] as CFArray

guard let gradient = CGGradient(colorsSpace: colorSpace, colors: gradientColors, locations: [0.0, 1.0]) else {
    fputs("Failed to create gradient\n", stderr)
    exit(1)
}
ctx.drawLinearGradient(gradient, start: CGPoint(x: 512, y: margin), end: CGPoint(x: 512, y: margin + bodySize), options: [])

// 3. Draw Centered BitChord Logo Mark
if let markImage = NSImage(contentsOfFile: markPath),
   let cgMark = markImage.cgImage(forProposedRect: nil, context: nil, hints: nil) {
    let markW: CGFloat = 585
    let markH: CGFloat = markW * (CGFloat(cgMark.height) / CGFloat(cgMark.width))
    let markX: CGFloat = (canvasSize - markW) / 2
    let markY: CGFloat = (canvasSize - markH) / 2
    let markRect = CGRect(x: markX, y: markY, width: markW, height: markH)
    ctx.draw(cgMark, in: markRect)
} else {
    fputs("Warning: Could not load mark from \(markPath)\n", stderr)
}
ctx.restoreGState()

// 4. Subtle Inner Rim Highlight (Apple standard for depth on dark/light mode)
ctx.saveGState()
ctx.addPath(squirclePath)
ctx.setLineWidth(1.5)
ctx.setStrokeColor(CGColor(red: 1.0, green: 1.0, blue: 1.0, alpha: 0.15))
ctx.strokePath()
ctx.restoreGState()

// 5. Export PNG
guard let outputCGImage = ctx.makeImage() else {
    fputs("Failed to generate final image\n", stderr)
    exit(1)
}

let rep = NSBitmapImageRep(cgImage: outputCGImage)
guard let pngData = rep.representation(using: .png, properties: [:]) else {
    fputs("Failed to convert image to PNG\n", stderr)
    exit(1)
}

do {
    try pngData.write(to: URL(fileURLWithPath: outputPath))
    print("Generated macOS icon: \(outputPath)")
} catch {
    fputs("Failed to write to \(outputPath): \(error)\n", stderr)
    exit(1)
}

