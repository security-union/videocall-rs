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
        sans: ['-apple-system', 'BlinkMacSystemFont', '"SF Pro Display"', '"Helvetica Neue"', 'system-ui', 'sans-serif'],
        mono: ['"SF Mono"', '"Fira Code"', 'Menlo', 'Monaco', 'monospace'],
      },
      colors: {
        primary: {
          DEFAULT: "#2997ff",
          dark: "#0077ed",
        },
        background: {
          DEFAULT: "#000000",
          secondary: "#1d1d1f",
          tertiary: "#2d2d2f",
        },
        foreground: {
          DEFAULT: "#f5f5f7",
          secondary: "rgba(255,255,255,0.5)",
          tertiary: "rgba(255,255,255,0.3)",
          quaternary: "rgba(255,255,255,0.16)",
        },
        border: {
          DEFAULT: "rgba(255,255,255,0.08)",
          secondary: "rgba(255,255,255,0.14)",
        },
        success: "#30d158",
        warning: "#ff9f0a",
        error:   "#ff453a",
        info:    "#2997ff",
      },
      borderRadius: {
        'none': '0',
        'sm': '0.25rem',
        'DEFAULT': '0.5rem',
        'md': '0.75rem',
        'lg': '1rem',
        'xl': '1.25rem',
        '2xl': '1.5rem',
        '3xl': '2rem',
        'full': '9999px',
      },
    },
  },
  plugins: [],
};
