import Foundation
import VideoCallIOS

// Simple test of the Rust bindings
func testVideocallBinding() {
    print("Testing Rust bindings...")
    let greeting = videocall.hello_world()
    let version = videocall.get_version()
    
    print("Greeting: \(greeting)")
    print("Version: \(version)")
}

// Run the test
testVideocallBinding() 