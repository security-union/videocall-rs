//
//  ContentView.swift
//  videocall-demo
//
//  Created by Dario Lencina on 4/1/25.
//

import SwiftUI

struct ContentView: View {
    var body: some View {
        VStack {
            Image(systemName: "globe")
                .imageScale(.large)
                .foregroundStyle(.tint)
            Text(helloWorld())
        }
        .padding()
    }
}

#Preview {
    ContentView()
}


