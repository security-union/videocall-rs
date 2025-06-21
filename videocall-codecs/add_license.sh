#!/bin/bash
# A script to add an Apache 2.0 license header to all Rust source files.

# --- Configuration ---
# The copyright holder for the license notice.
COPYRIGHT_HOLDER="Security Union LLC"
# The string to search for to determine if a license header already exists.
# This makes the script safe to run multiple times.
CHECK_STRING="Copyright"

# The full license text to be prepended to files.
# Using a HEREDOC for multiline text is clean and readable.
read -r -d '' LICENSE_TEXT << EOM
/*
 * Copyright $(date +%Y) ${COPYRIGHT_HOLDER}
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
EOM

# --- Script Logic ---
# Use find to locate all files ending in .rs, then loop through them.
# -print0 and `while read -d ''` handle filenames with spaces or special characters.
find . -type f -name "*.rs" -print0 | while IFS= read -r -d '' file; do
    # Check if the file already contains the license check string.
    if ! grep -q "$CHECK_STRING" "$file"; then
        echo "Adding license to $file"
        # Create a temporary file to work with.
        TEMP_FILE=$(mktemp)
        # Write the license header to the temp file.
        echo "$LICENSE_TEXT" > "$TEMP_FILE"
        # Add a blank line for style.
        echo "" >> "$TEMP_FILE"
        # Append the original file's content after the header.
        cat "$file" >> "$TEMP_FILE"
        # Replace the original file with the new, licensed version.
        mv "$TEMP_FILE" "$file"
    else
        echo "License already exists in $file, skipping."
    fi
done

echo "License script finished." 