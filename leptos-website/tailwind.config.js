/** @type {import('tailwindcss').Config} */
module.exports = {
  content: ["*.html", "./src/**/*.rs"],
  darkMode: "class",
  theme: {
    extend: {
      screens: {
        "4xl": "1920px",
      },
      fontFamily: {
        // System grotesque for display/body — ships zero font bytes.
        sans: ['"Helvetica Neue"', "Helvetica", "Arial", '"Segoe UI"', "system-ui", "sans-serif"],
        // Monospace does the instrumentation work: indices, readouts, code.
        mono: ['"SF Mono"', '"JetBrains Mono"', '"Fira Code"', "Menlo", "Monaco", "monospace"],
      },
      colors: {
        // Warm-neutral near-black ink/surface ramp.
        bg: { DEFAULT: "#0A0A0B", s1: "#111113", s2: "#17171A", code: "#0D0D0F" },
        // Warm grey foreground ramp.
        fg: { DEFAULT: "#F2F2F0", 2: "#A1A1A0", 3: "#6E6E6D", 4: "#3A3A3B" },
        // Oxide signal accent — live/active/focus only.
        signal: { DEFAULT: "#D96B3C", quiet: "rgba(217,107,60,0.14)" },
        // Hairlines.
        line: { DEFAULT: "rgba(242,242,240,0.10)", strong: "rgba(242,242,240,0.18)" },
        // Status readouts only, never decoration.
        ok: "#8FA98A",
        warn: "#C9A227",
        err: "#B5493B",
      },
      borderRadius: {
        none: "0",
        sm: "2px",
        DEFAULT: "4px",
        md: "4px",
        panel: "6px",
        full: "9999px",
      },
      fontSize: {
        eyebrow: ["0.75rem", { lineHeight: "1", letterSpacing: "0.16em" }],
        data: ["0.8125rem", { lineHeight: "1.4", letterSpacing: "0.02em" }],
      },
      maxWidth: {
        content: "1200px",
      },
    },
  },
  plugins: [],
};
