.PHONY: dev clean install

# Development command that runs both Tailwind CSS watch and cargo leptos watch
dev:
	@echo "🚀 Starting development servers..."
	@echo "📦 Tailwind CSS: Watching for changes..."
	@echo "🦀 Leptos: Starting watch mode..."
	@echo "Press Ctrl+C to stop both processes"
	@echo ""
	@trap 'echo ""; echo "🛑 Stopping development servers..."; kill 0; exit 0' INT; \
	npx tailwindcss -i input.css -o style/output.css --config tailwind.config.js --watch & \
	cargo leptos watch & \
	wait

# Install dependencies
install:
	@echo "📦 Installing Node.js dependencies..."
	npm install
	@echo "🦀 Installing Rust dependencies..."
	cargo build
	@echo "✅ All dependencies installed!"

# Clean build artifacts
clean:
	@echo "🧹 Cleaning build artifacts..."
	rm -rf target/
	rm -rf node_modules/
	rm -f style/output.css
	@echo "✅ Clean complete!"

# Build production assets
build:
	@echo "🏗️  Building production assets..."
	npx tailwindcss -i input.css -o style/output.css --config tailwind.config.js --minify
	cargo leptos build
	@echo "✅ Production build complete!"

# Help command
help:
	@echo "Available commands:"
	@echo "  dev     - Start development servers (Tailwind + Leptos watch)"
	@echo "  install - Install all dependencies"
	@echo "  build   - Build production assets"
	@echo "  clean   - Clean all build artifacts"
	@echo "  help    - Show this help message"