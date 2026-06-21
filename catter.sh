#!/bin/bash

OUTPUT_FILE="combined_output.txt"
TARGET_DIR="${1:-.}"

> "$OUTPUT_FILE"

find "$TARGET_DIR" -type f | sort | while read -r file; do
  relative_path="${file#$TARGET_DIR/}"
  echo "=== $relative_path ===" >> "$OUTPUT_FILE"
  echo "" >> "$OUTPUT_FILE"
  cat "$file" >> "$OUTPUT_FILE"
  echo "" >> "$OUTPUT_FILE"
  echo "" >> "$OUTPUT_FILE"
done

echo "Done! Output written to $OUTPUT_FILE"