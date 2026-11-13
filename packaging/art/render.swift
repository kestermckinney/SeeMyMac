// Renders an SVG to a PNG or BMP of an exact pixel size, using only what ships with macOS.
//
//   swift packaging/art/render.swift <in.svg> <out.png|out.bmp> <width> <height> [dpi]
//
// packaging/art/render.sh calls this to regenerate the installer artwork; the results are
// committed so builds on Windows (and in CI) do not need it.
import AppKit

let args = CommandLine.arguments
guard args.count >= 5, let w = Int(args[3]), let h = Int(args[4]) else {
    FileHandle.standardError.write("usage: render.swift <in.svg> <out.png|out.bmp> <width> <height> [dpi]\n".data(using: .utf8)!)
    exit(2)
}
let dpi = args.count > 5 ? Double(args[5]) ?? 72 : 72
guard let image = NSImage(contentsOfFile: args[1]) else {
    FileHandle.standardError.write("could not read \(args[1])\n".data(using: .utf8)!)
    exit(1)
}
let bmp = args[2].lowercased().hasSuffix(".bmp")
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: w, pixelsHigh: h, bitsPerSample: 8,
                           samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                           colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
// Point size sets the PNG's resolution: a 1240 px image at 144 dpi is drawn 620 pt wide (Retina).
rep.size = NSSize(width: Double(w) * 72 / dpi, height: Double(h) * 72 / dpi)
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
NSGraphicsContext.current?.imageInterpolation = .high
if bmp {   // opaque: transparent corners become the white of the wizard's header strip
    NSColor.white.setFill()
    NSRect(origin: .zero, size: rep.size).fill()
}
image.draw(in: NSRect(origin: .zero, size: rep.size))
NSGraphicsContext.restoreGraphicsState()
if bmp {
    // A plain 24-bit, bottom-up BMP (BITMAPINFOHEADER): the format Inno Setup loads most reliably.
    let row = (w * 3 + 3) & ~3
    var out = Data()
    func le32(_ v: Int) { withUnsafeBytes(of: UInt32(truncatingIfNeeded: v).littleEndian) { out.append(contentsOf: $0) } }
    func le16(_ v: Int) { withUnsafeBytes(of: UInt16(truncatingIfNeeded: v).littleEndian) { out.append(contentsOf: $0) } }
    out.append(contentsOf: [0x42, 0x4D]); le32(54 + row * h); le32(0); le32(54)
    le32(40); le32(w); le32(h); le16(1); le16(24); le32(0); le32(row * h); le32(2835); le32(2835); le32(0); le32(0)
    for y in stride(from: h - 1, through: 0, by: -1) {
        var line = [UInt8](repeating: 0, count: row)
        for x in 0..<w {
            let c = rep.colorAt(x: x, y: y)!.usingColorSpace(.deviceRGB)!
            line[x * 3] = UInt8((c.blueComponent * 255).rounded())
            line[x * 3 + 1] = UInt8((c.greenComponent * 255).rounded())
            line[x * 3 + 2] = UInt8((c.redComponent * 255).rounded())
        }
        out.append(contentsOf: line)
    }
    try! out.write(to: URL(fileURLWithPath: args[2]))
} else {
    try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: args[2]))
}
