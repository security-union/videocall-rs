// The Swift Programming Language
// https://docs.swift.org/swift-book

import Foundation
import videocallFFI

/// A Swift wrapper for the WebTransport client that provides a high-level interface
/// for WebTransport operations.
public class WebTransportClientWrapper {
    private let client: WebTransportClient
    private let datagramQueue: DatagramQueue
    
    /// Initialize a new WebTransport client
    public init() {
        self.client = WebTransportClient()
        self.datagramQueue = DatagramQueue()
    }
    
    /// Connect to a WebTransport server
    /// - Parameter url: The URL of the WebTransport server
    /// - Throws: WebTransportError if the connection fails
    public func connect(url: String) throws {
        try client.connect(url: url)
    }
    
    /// Send a datagram to the server
    /// - Parameter data: The data to send
    /// - Throws: WebTransportError if sending fails
    public func sendDatagram(data: [UInt8]) throws {
        try client.sendDatagram(data: data)
    }
    
    /// Send a text message as a datagram
    /// - Parameter message: The text message to send
    /// - Throws: WebTransportError if sending fails
    public func sendTextMessage(_ message: String) throws {
        let data = Array(message.utf8)
        try sendDatagram(data: data)
    }
    
    /// Start listening for datagrams
    /// - Throws: WebTransportError if subscription fails
    public func subscribeToDatagrams() throws {
        try client.subscribeToDatagrams(queue: datagramQueue)
    }
    
    /// Stop listening for datagrams
    /// - Throws: WebTransportError if stopping fails
    public func stopDatagramListener() throws {
        try client.stopDatagramListener()
    }
    
    /// Check if there are any datagrams available
    /// - Returns: True if there are datagrams available
    /// - Throws: WebTransportError if checking fails
    public func hasDatagrams() throws -> Bool {
        try datagramQueue.hasDatagrams()
    }
    
    /// Receive a datagram from the queue
    /// - Returns: The received data
    /// - Throws: WebTransportError if receiving fails
    public func receiveDatagram() throws -> [UInt8] {
        try datagramQueue.receiveDatagram()
    }
    
    /// Receive a text message from the queue
    /// - Returns: The received text message, or nil if the data is not valid UTF-8
    /// - Throws: WebTransportError if receiving fails
    public func receiveTextMessage() throws -> String? {
        let data = try receiveDatagram()
        return String(data: Data(data), encoding: .utf8)
    }
    
    /// Disconnect from the WebTransport server
    public func disconnect() {
        // TODO: Add disconnect method to the Rust code
    }
}
