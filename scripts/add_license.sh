#!/bin/bash

# --- Configuration ---
COPYRIGHT_HOLDER="Security Union LLC"

# The new dual license text
read -r -d '' NEW_LICENSE_TEXT << EOM
/*
 * Copyright $(date +%Y) ${COPYRIGHT_HOLDER}
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */
EOM

# Function to remove license block from /* to */
remove_license_block() {
    local file="$1"
    local temp_file=$(mktemp)
    
    # Use awk to remove from first /* until first */
    awk '
    BEGIN { 
        in_license_block = 0
        found_start = 0
    }
    /^\/\*/ && !found_start { 
        in_license_block = 1
        found_start = 1
        next 
    }
    in_license_block && /\*\// { 
        in_license_block = 0
        next 
    }
    !in_license_block { print }
    ' "$file" > "$temp_file"
    
    mv "$temp_file" "$file"
}

# Main logic - process all .rs files
find . -type f -name "*.rs" -print0 | while IFS= read -r -d '' file; do
    echo "Processing $file"
    
    # Check if file starts with a license block
    if head -1 "$file" | grep -q "^/\*"; then
        echo "  Removing existing license block"
        remove_license_block "$file"
    fi
    
    # Always add the new license header
    temp_file=$(mktemp)
    echo "$NEW_LICENSE_TEXT" > "$temp_file"
    echo "" >> "$temp_file"  # Add blank line after license
    cat "$file" >> "$temp_file"
    mv "$temp_file" "$file"
    
    echo "  Added new dual license header"
done

echo "License update complete!"