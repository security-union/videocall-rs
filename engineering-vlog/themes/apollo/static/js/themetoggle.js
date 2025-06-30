function setTheme(mode) {
    localStorage.setItem("theme-storage", mode);
}

// Functions needed for the theme toggle
//

function toggleTheme() {
    if (localStorage.getItem("theme-storage") === "light") {
        setTheme("dark");
        updateItemToggleTheme();
    } else if (localStorage.getItem("theme-storage") === "dark") {
        setTheme("light");
        updateItemToggleTheme();
    }
}

function updateItemToggleTheme() {
    let mode = getSavedTheme();

    const darkModeStyle = document.getElementById("darkModeStyle");
    if (darkModeStyle) {
        darkModeStyle.disabled = (mode === "light");
    }

    const sunIcon = document.getElementById("sun-icon");
    const moonIcon = document.getElementById("moon-icon");
    if (sunIcon && moonIcon) {
        sunIcon.style.display = (mode === "dark") ? "block" : "none";
        moonIcon.style.display = (mode === "light") ? "block" : "none";
    }

    let htmlElement = document.querySelector("html");
    if (mode === "dark") {
        htmlElement.classList.remove("light")
        htmlElement.classList.add("dark")
    } else if (mode === "light") {
        htmlElement.classList.remove("dark")
        htmlElement.classList.add("light")
    }
}

function getSavedTheme() {
    let currentTheme = localStorage.getItem("theme-storage");
    if(!currentTheme) {
        if (window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches) {
            currentTheme = "dark";
        } else {
            currentTheme = "light";
        }
    }

    return currentTheme;
}

// Update the toggle theme on page load
updateItemToggleTheme();
