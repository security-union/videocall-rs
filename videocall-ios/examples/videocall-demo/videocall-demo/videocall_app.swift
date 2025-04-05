//
//  videocall_app.swift
//  videocall-demo
//
//  Created by Dario Lencina on 4/1/25.
//

import SwiftUI

@main
struct VideocallDemoApp: App {
    init() {
        // Call getVersion when the app initializes
        let version = getLibraryVersion()
        print("VideoCallIOS library version: \(version)")
    }
    
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
    
    // Method to get the version from the Rust library
    func getLibraryVersion() -> String {
        return getVersion()
    }
}


