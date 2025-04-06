import XCTest
@testable import VideoCallKit

final class VideoCallKitTests: XCTestCase {
    func testWebTransportClientWrapper() throws {
        let client = WebTransportClientWrapper()
        
        // Test initialization
        XCTAssertNotNil(client)
        
        // Test connection
        XCTAssertNoThrow(try client.connect(url: "wss://example.com"))
        
        // Test datagram operations
        XCTAssertNoThrow(try client.subscribeToDatagrams())
        XCTAssertNoThrow(try client.stopDatagramListener())
        
        // Test message sending
        XCTAssertNoThrow(try client.sendTextMessage("test"))
        
        // Test disconnection
        client.disconnect()
    }
}
