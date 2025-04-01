//
//  ContentView.swift
//  videocall-demo
//
//  Created by Dario Lencina on 4/1/25.
//

import SwiftUI
import videocallFFI

struct WebTransportView: View {
    @State private var url: String = "https://transport.rustlemania.com"
    @State private var connectionStatus: String = "Not connected"
    @State private var isConnecting: Bool = false
    @State private var message: String = "Hello WebTransport!"
    @State private var responseText: String = ""
    
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
            
            Text(connectionStatus)
                .padding()
                .background(
                    RoundedRectangle(cornerRadius: 8)
                        .fill(connectionStatus == "Connected successfully!" ? Color.green.opacity(0.2) : 
                              connectionStatus == "Not connected" ? Color.gray.opacity(0.2) : 
                              Color.red.opacity(0.2))
                )
            
            if connectionStatus == "Connected successfully!" {
                TextField("Message to send", text: $message)
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
                
                if !responseText.isEmpty {
                    Text("Response: \(responseText)")
                        .padding()
                        .background(
                            RoundedRectangle(cornerRadius: 8)
                                .fill(Color.blue.opacity(0.1))
                        )
                }
            }
        }
        .padding()
    }
    
    private func connectToServer() {
        isConnecting = true
        connectionStatus = "Connecting..."
        
        print("🔄 Starting WebTransport connection to: \(url)")
        
        // Use DispatchQueue to avoid blocking the UI
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                print("🔍 Creating WebTransportClient with URL: \(url)")
                let client = WebTransportClient(url: url)
                
                print("🚀 Calling connect() method")
                try client.connect()
                
                print("✅ Connection successful!")
                
                // Update UI on the main thread
                DispatchQueue.main.async {
                    print("📱 Updating UI - connection successful")
                    connectionStatus = "Connected successfully!"
                    isConnecting = false
                }
            } catch let error as WebTransportError {
                // Handle specific WebTransportError types
                print("❌ Caught WebTransportError: \(error)")
                
                DispatchQueue.main.async {
                    switch error {
                    case .ConnectionFailed(let message):
                        print("⚠️ Connection failed: \(message)")
                        connectionStatus = "Connection failed: \(message)"
                    case .InvalidUrl(let message):
                        print("⚠️ Invalid URL: \(message)")
                        connectionStatus = "Invalid URL: \(message)"
                    case .StreamError(let message):
                        print("⚠️ Stream error: \(message)")
                        connectionStatus = "Stream error: \(message)"
                    case .HttpError(let message):
                        print("⚠️ HTTP error: \(message)")
                        connectionStatus = "HTTP error: \(message)"
                    case .TlsError(let message):
                        print("⚠️ TLS error: \(message)")
                        connectionStatus = "TLS error: \(message)"
                    case .Unknown(let message):
                        print("⚠️ Unknown error: \(message)")
                        connectionStatus = "Unknown error: \(message)"
                    }
                    isConnecting = false
                }
            } catch {
                print("❓ Unexpected error: \(error)")
                
                // Update UI on the main thread
                DispatchQueue.main.async {
                    connectionStatus = "Error: \(error.localizedDescription)"
                    isConnecting = false
                }
            }
        }
    }
    
    private func sendDatagram() {
        print("📤 Sending datagram: \(message)")
        
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let client = WebTransportClient(url: url)
                
                // We need to connect first since our implementation doesn't store the session
                try client.connect()
                
                // Convert string to bytes
                if let data = message.data(using: .utf8) {
                    let bytes = [UInt8](data)
                    
                    print("📦 Sending \(bytes.count) bytes")
                    let response = try client.sendDatagram(data: bytes)
                    
                    // Convert response bytes back to string if possible
                    if let responseString = String(data: Data(response), encoding: .utf8) {
                        print("📥 Received response: \(responseString)")
                        
                        DispatchQueue.main.async {
                            responseText = responseString
                        }
                    } else {
                        print("⚠️ Could not decode response as string")
                        
                        DispatchQueue.main.async {
                            responseText = "Binary data received (\(response.count) bytes)"
                        }
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
