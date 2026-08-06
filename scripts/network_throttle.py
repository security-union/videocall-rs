#!/usr/bin/env python3
import argparse
import subprocess
import sys
import os
import signal
import atexit
import urllib.parse

def check_root():
    if os.geteuid() != 0:
        print("This script must be run as root (use sudo)")
        sys.exit(1)

def cleanup():
    print("\nCleaning up PF rules...")
    # Flush the specific anchor we created
    subprocess.run(["pfctl", "-a", "throttle", "-F", "all"], check=False)
    # Flush dummynet rules
    subprocess.run(["dnctl", "-q", "flush"], check=False)
    print("Cleanup complete")

def setup_pf_and_dummynet(target_host, download_rate, upload_rate):
    # Step 1: Flush any existing dummynet configuration
    subprocess.run(["dnctl", "-q", "flush"], check=True)
    
    # Step 2: Create the pipes for upload and download
    # Download pipe
    subprocess.run([
        "dnctl", "pipe", "1", "config",
        "bw", f"{download_rate}Kbit/s", 
        "queue", "10"
    ], check=True)
    
    # Upload pipe
    subprocess.run([
        "dnctl", "pipe", "2", "config",
        "bw", f"{upload_rate}Kbit/s",
        "queue", "10"
    ], check=True)
    
    # Step 3: Set up PF to use dummynet
    # First, add the dummynet anchor to the main config
    subprocess.run([
        "sh", "-c",
        f"(cat /etc/pf.conf && echo 'dummynet-anchor \"throttle\"' && echo 'anchor \"throttle\"') | pfctl -f -"
    ], check=True)
    
    # Step 4: Configure the anchor with our rules
    pf_rules = f"""
# Skip localhost traffic
no dummynet quick on lo0 all

# Throttle TCP traffic to specified host
dummynet in proto tcp from any to {target_host} pipe 1
dummynet out proto tcp from any to {target_host} pipe 2

# Throttle UDP traffic to specified host
dummynet in proto udp from any to {target_host} pipe 1
dummynet out proto udp from any to {target_host} pipe 2
"""
    
    with open("/tmp/pf.rules.throttle", "w") as f:
        f.write(pf_rules)
    
    # Load the anchor-specific rules
    subprocess.run([
        "pfctl", "-a", "throttle", "-f", "/tmp/pf.rules.throttle"
    ], check=True)
    
    # Step 5: Enable PF
    subprocess.run(["pfctl", "-E"], check=False)

def main():
    parser = argparse.ArgumentParser(description='Throttle network connection to a specific URL')
    parser.add_argument('url', help='Target URL to throttle')
    parser.add_argument('--download', type=int, default=1000,
                      help='Download speed in Kbit/s (default: 1000)')
    parser.add_argument('--upload', type=int, default=1000,
                      help='Upload speed in Kbit/s (default: 1000)')
    
    args = parser.parse_args()

    # Extract hostname from URL
    parsed_url = urllib.parse.urlparse(args.url)
    target_host = parsed_url.hostname
    
    if not target_host:
        print("Invalid URL provided")
        sys.exit(1)

    check_root()
    
    # Register cleanup function
    atexit.register(cleanup)
    signal.signal(signal.SIGINT, lambda x, y: sys.exit(0))

    try:
        print(f"Setting up throttling for {target_host}")
        print(f"Download speed: {args.download} Kbit/s")
        print(f"Upload speed: {args.upload} Kbit/s")
        
        # Setup PF and dummynet
        setup_pf_and_dummynet(target_host, args.download, args.upload)
        
        print("\nThrottling is now active. Press Ctrl+C to stop and cleanup.")
        
        # Keep the script running
        while True:
            signal.pause()
            
    except subprocess.CalledProcessError as e:
        print(f"Error occurred: {e}")
        sys.exit(1)

if __name__ == "__main__":
    main() 