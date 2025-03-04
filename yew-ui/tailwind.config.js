/** @type {import('tailwindcss').Config} */
module.exports = {
  content: {
    files: ["*.html", "./src/**/*.rs"],
  },
  darkMode: "class",

  theme: {
    extend: {
      screens: {
        "4xl": "1920px",
      },
      fontFamily: {
        sans: ['Inter', 'system-ui', 'sans-serif'],
        mono: ['Fira Code', 'monospace'],
      },
    },
    colors: {
      transparent: "transparent",
      current: "currentColor",
      primary: "#7928CA",    // Purple
      secondary: "#38BDF8",  // Blue
      accent: "#FF3366",     // Pink/Red accent
      background: {
        DEFAULT: "#0D131F",  // Dark background
        light: "#1A2333",    // Lighter dark background for cards/sections
      },
      foreground: {
        DEFAULT: "#FFFFFF",  // Primary text color
        muted: "#D1D5DB",    // Secondary text color
        subtle: "#9CA3AF",   // Tertiary text color
      },
      white: "#FFFFFF",
      black: "#000000",
      gray: {
        50: "#F9FAFB",
        100: "#F3F4F6",
        200: "#E5E7EB",
        300: "#D1D5DB",
        400: "#9CA3AF",
        500: "#6B7280",
        600: "#4B5563",
        700: "#374151",
        800: "#1F2937",
        900: "#111827",
      },
      // Semantic colors
      success: "#10B981",    // Green
      warning: "#F59E0B",    // Amber
      error: "#EF4444",      // Red
      info: "#3B82F6",       // Blue
    },
    // Typography scale
    fontSize: {
      xs: ["0.75rem", { lineHeight: "1rem" }],
      sm: ["0.875rem", { lineHeight: "1.25rem" }],
      base: ["1rem", { lineHeight: "1.5rem" }],
      lg: ["1.125rem", { lineHeight: "1.75rem" }],
      xl: ["1.25rem", { lineHeight: "1.75rem" }],
      "2xl": ["1.5rem", { lineHeight: "2rem" }],
      "3xl": ["1.875rem", { lineHeight: "2.25rem" }],
      "4xl": ["2.25rem", { lineHeight: "2.5rem" }],
      "5xl": ["3rem", { lineHeight: "1" }],
      "6xl": ["3.75rem", { lineHeight: "1" }],
    },
  },
  plugins: [],
}
