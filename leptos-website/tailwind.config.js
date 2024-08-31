/** @type {import('tailwindcss').Config} */
module.exports = {
  content: {
    files: ["*.html", "./src/**/*.rs"],
  },
  //darkMode: "class",

  theme: {
    extend: {
      screens: {
        "4xl": "1920px",
      },
    },
    colors: {
      transparent: "transparent",
      red: "#EF3939",
      pink: "#F0ADA8",
      eggshell: "#F1FAEE",
      white: "#F2F8FA",
      light_blue: "#A8DADC",
      beige: "#D2D7B4",
      dark_blue: "#324571",
      purple: "#181139",
      black: "#0F191C",
    },
    // fontSize: {
    //   sm: "0.8rem",
    //   base: "1rem",
    //   lg: "1.2rem",
    //   xl: "1.5rem",
    //   "2xl": "1.8rem",
    //   "3xl": "2.4rem",
    //   "4xl": "3.2rem",
    //   "5xl": "4.8rem",
    //   "6xl": "6.4rem",
    // },
  },
};
