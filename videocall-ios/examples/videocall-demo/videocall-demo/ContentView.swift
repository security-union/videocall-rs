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
            
            Text(responseText)
                .padding()
                .frame(maxWidth: .infinity)
                .background(Color.gray.opacity(0.2))
                .cornerRadius(10)
                .padding(.horizontal)
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
                print("🔍 Creating WebTransportClient")
                let client = WebTransportClient()
                
                print("🚀 Calling connect() method")
                try client.connect(url: url)
                
                print("✅ Connection successful!")
                
                // Update UI on the main thread
                DispatchQueue.main.async {
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
        
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let client = WebTransportClient()
                
                // We need to connect first since our implementation doesn't store the session
                try client.connect(url: url)
                
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
