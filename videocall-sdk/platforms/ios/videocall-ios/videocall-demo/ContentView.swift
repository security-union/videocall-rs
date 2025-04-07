//
//  ContentView.swift
//  videocall-demo
//
//  Created by Dario Lencina on 4/1/25.
//

import SwiftUI
import videocallFFI

struct WebTransportView: View {
    @State private var url: String = "https://echo.webtransport.rs"
    @State private var connectionStatus: String = "Not connected"
    @State private var isConnecting: Bool = false
    @State private var message: String = "Hello WebTransport!"
    @State private var responseText: String = ""
    @State private var receivedDatagrams: [String] = []
    @State private var isSubscribed: Bool = false
    @State private var client: WebTransportClient? = nil
    @State private var datagramQueue: DatagramQueue? = nil
    @State private var datagramTimer: Timer? = nil
    
    var body: some View {
        VStack(spacing: 20) {
            Text("WebTransport Test")
                .font(.title)
                .padding()
            
            TextField("Server URL", text: $url)
                .textFieldStyle(RoundedBorderTextFieldStyle())
                .padding(.horizontal)
                .autocapitalization(.none)
                .disableAutocorrection(true)
            
            Button(action: {
                connectToServer()
            }) {
                Text(isConnecting ? "Connecting..." : "Connect")
                    .frame(minWidth: 200)
                    .padding()
                    .background(isConnecting ? Color.gray : Color.blue)
                    .foregroundColor(.white)
                    .cornerRadius(10)
            }
            .disabled(isConnecting)
            
            TextField("Message", text: $message)
                .textFieldStyle(RoundedBorderTextFieldStyle())
                .padding(.horizontal)
            
            Button(action: {
                sendDatagram()
            }) {
                Text("Send Datagram")
                    .frame(minWidth: 200)
                    .padding()
                    .background(Color.green)
                    .foregroundColor(.white)
                    .cornerRadius(10)
            }
            
            Button(action: {
                if isSubscribed {
                    stopDatagramListener()
                } else {
                    subscribeToDatagrams()
                }
            }) {
                Text(isSubscribed ? "Stop Listening" : "Listen for Datagrams")
                    .frame(minWidth: 200)
                    .padding()
                    .background(isSubscribed ? Color.red : Color.orange)
                    .foregroundColor(.white)
                    .cornerRadius(10)
            }
            
            Text("Received Datagrams:")
                .font(.headline)
                .padding(.top)
            
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    ForEach(receivedDatagrams, id: \.self) { datagram in
                        Text(datagram)
                            .padding(8)
                            .background(Color.blue.opacity(0.1))
                            .cornerRadius(5)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding()
            }
            .frame(height: 200)
            .background(Color.gray.opacity(0.1))
            .cornerRadius(10)
            .padding(.horizontal)
            
            Text(responseText)
                .padding()
                .frame(maxWidth: .infinity)
                .background(Color.gray.opacity(0.2))
                .cornerRadius(10)
                .padding(.horizontal)
        }
        .padding()
        .onDisappear {
            stopDatagramTimer()
        }
    }
    
    private func connectToServer() {
        isConnecting = true
        connectionStatus = "Connecting..."
        
        print("🔄 Starting WebTransport connection to: \(url)")
        
        // Use DispatchQueue to avoid blocking the UI
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                print("🔍 Creating WebTransportClient")
                let newClient = WebTransportClient()
                
                print("🚀 Calling connect() method")
                try newClient.connect(url: url)
                
                print("✅ Connection successful!")
                
                // Store the client instance
                DispatchQueue.main.async {
                    self.client = newClient
                    print("📱 Updating UI - connection successful")
                    connectionStatus = "Connected successfully!"
                    isConnecting = false
                }
                
            } catch let error as WebTransportError {
                print("❌ Connection error: \(error)")
                
                DispatchQueue.main.async {
                    connectionStatus = "Connection failed: \(error)"
                    isConnecting = false
                }
            } catch {
                print("❓ Unexpected error: \(error)")
                
                DispatchQueue.main.async {
                    connectionStatus = "Unexpected error: \(error.localizedDescription)"
                    isConnecting = false
                }
            }
        }
    }
    
    private func sendDatagram() {
        print("📤 Sending datagram: \(message)")
        
        guard let client = client else {
            print("❌ No active client connection")
            DispatchQueue.main.async {
                responseText = "Error: Not connected to server"
            }
            return
        }
        
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                // Convert string to bytes
                if let data = message.data(using: .utf8) {
                    let bytes = [UInt8](data)
                    
                    print("📦 Sending \(bytes.count) bytes")
                    try client.sendDatagram(data: bytes)
                    
                    DispatchQueue.main.async {
                        responseText = "Datagram sent successfully"
                    }
                }
            } catch let error as WebTransportError {
                print("❌ Error sending datagram: \(error)")
                
                DispatchQueue.main.async {
                    responseText = "Error sending datagram: \(error)"
                }
            } catch {
                print("❓ Unexpected error sending datagram: \(error)")
                
                DispatchQueue.main.async {
                    responseText = "Unexpected error: \(error.localizedDescription)"
                }
            }
        }
    }
    
    private func startDatagramTimer() {
        // Create a timer that checks for new datagrams every 100ms
        datagramTimer = Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { _ in
            checkForDatagrams()
        }
    }
    
    private func stopDatagramTimer() {
        datagramTimer?.invalidate()
        datagramTimer = nil
    }
    
    private func checkForDatagrams() {
        guard let queue = datagramQueue else { return }
        
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                while try queue.hasDatagrams() {
                    let data = try queue.receiveDatagram()
                    if let string = String(data: Data(data), encoding: .utf8) {
                        DispatchQueue.main.async {
                            self.receivedDatagrams.append(string)
                        }
                    } else {
                        DispatchQueue.main.async {
                            self.receivedDatagrams.append("Binary data: \(data.count) bytes")
                        }
                    }
                }
            } catch {
                print("Error checking datagrams: \(error)")
            }
        }
    }
    
    private func subscribeToDatagrams() {
        print("👂 Subscribing to datagrams")
        
        guard let client = client else {
            print("❌ No active client connection")
            DispatchQueue.main.async {
                responseText = "Error: Not connected to server"
            }
            return
        }
        
        // Create a new datagram queue
        let queue = DatagramQueue()
        datagramQueue = queue
        
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                // Subscribe to datagrams with the queue
                try client.subscribeToDatagrams(queue: queue)
                
                DispatchQueue.main.async {
                    isSubscribed = true
                    responseText = "Listening for datagrams..."
                    startDatagramTimer()
                }
            } catch let error as WebTransportError {
                print("❌ Error subscribing to datagrams: \(error)")
                
                DispatchQueue.main.async {
                    responseText = "Error subscribing to datagrams: \(error)"
                }
            } catch {
                print("❓ Unexpected error subscribing to datagrams: \(error)")
                
                DispatchQueue.main.async {
                    responseText = "Unexpected error: \(error.localizedDescription)"
                }
            }
        }
    }
    
    private func stopDatagramListener() {
        print("🛑 Stopping datagram listener")
        
        guard let client = client else {
            print("❌ No active client connection")
            return
        }
        
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                try client.stopDatagramListener()
                stopDatagramTimer()
                
                DispatchQueue.main.async {
                    isSubscribed = false
                    responseText = "Stopped listening for datagrams"
                }
            } catch let error as WebTransportError {
                print("❌ Error stopping datagram listener: \(error)")
                
                DispatchQueue.main.async {
                    responseText = "Error stopping datagram listener: \(error)"
                }
            } catch {
                print("❓ Unexpected error stopping datagram listener: \(error)")
                
                DispatchQueue.main.async {
                    responseText = "Unexpected error: \(error.localizedDescription)"
                }
            }
        }
    }
}

struct ContentView: View {
    var body: some View {
        TabView {
            VStack {
                Image(systemName: "globe")
                    .imageScale(.large)
                    .foregroundStyle(.tint)
                Text("space balls")
            }
            .padding()
            .tabItem {
                Label("Hello", systemImage: "globe")
            }
            
            WebTransportView()
                .tabItem {
                    Label("WebTransport", systemImage: "network")
                }
        }
    }
}

#Preview {
    ContentView()
} 
