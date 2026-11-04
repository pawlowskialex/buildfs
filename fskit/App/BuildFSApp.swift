import Foundation

// Host app for the buildfs FSKit module. Launching it once registers the
// embedded file system extension with the system.
@main
struct BuildFSApp {
    static func main() {
        print("buildfs FSKit host: extension registered; enable it in System Settings > General > Login Items & Extensions > File System Extensions")
    }
}
