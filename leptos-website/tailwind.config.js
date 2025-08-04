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
        sans: ['-apple-system', 'BlinkMacSystemFont', '"SF Pro Display"', '"SF Pro Text"', 'Inter', 'system-ui', 'sans-serif'],
        mono: ['SF Mono', '"Fira Code"', 'Menlo', 'Monaco', 'monospace'],
      },
      blur: {
        'xs': '2px',
      },
      backdropBlur: {
        'xs': '2px',
      },
      colors: {
        // Apple-inspired color palette
        primary: {
          DEFAULT: "#007AFF", // Apple blue
          dark: "#0A84FF",
        },
        
        // True Apple-style dark mode colors
        background: {
          DEFAULT: "#000000",  // Pure black like Apple
          secondary: "#1C1C1E", // Elevated surfaces
          tertiary: "#2C2C2E",  // Cards and modals
        },
        
        foreground: {
          DEFAULT: "#FFFFFF",  // Pure white text
          secondary: "#AEAEB2", // Secondary text
          tertiary: "#8E8E93",  // Tertiary text
          quaternary: "#636366", // Disabled text
        },
        
        border: {
          DEFAULT: "#38383A", // Subtle borders
          secondary: "#48484A", // More visible borders
        },
        
        // Semantic colors - Apple style
        success: "#30D158",    // Apple green
        warning: "#FF9F0A",    // Apple orange
        error: "#FF453A",      // Apple red
        info: "#007AFF",       // Apple blue
      },
      
      // Apple-inspired typography scale
      fontSize: {
        xs: ["0.75rem", { lineHeight: "1rem", fontWeight: "400" }],
        sm: ["0.875rem", { lineHeight: "1.25rem", fontWeight: "400" }],
        base: ["1rem", { lineHeight: "1.5rem", fontWeight: "400" }],
        lg: ["1.125rem", { lineHeight: "1.75rem", fontWeight: "400" }],
        xl: ["1.25rem", { lineHeight: "1.75rem", fontWeight: "400" }],
        "2xl": ["1.5rem", { lineHeight: "2rem", fontWeight: "500" }],
        "3xl": ["1.875rem", { lineHeight: "2.25rem", fontWeight: "600" }],
        "4xl": ["2.25rem", { lineHeight: "2.5rem", fontWeight: "600" }],
        "5xl": ["3rem", { lineHeight: "3.5rem", fontWeight: "700" }],
        "6xl": ["3.75rem", { lineHeight: "4rem", fontWeight: "700" }],
        "7xl": ["4.5rem", { lineHeight: "5rem", fontWeight: "700" }],
        "8xl": ["6rem", { lineHeight: "6.5rem", fontWeight: "700" }],
        "9xl": ["8rem", { lineHeight: "8.5rem", fontWeight: "700" }],
      },
      
      spacing: {
        '18': '4.5rem',
        '88': '22rem',
        '112': '28rem',
        '128': '32rem',
      },
      
      borderRadius: {
        'none': '0',
        'sm': '0.25rem',
        'DEFAULT': '0.5rem',
        'md': '0.75rem',
        'lg': '1rem',
        'xl': '1.5rem',
        '2xl': '2rem',
        '3xl': '3rem',
        'full': '9999px',
      },
    },
  },
  plugins: [],
};
