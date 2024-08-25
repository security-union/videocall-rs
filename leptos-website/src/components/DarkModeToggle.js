const classList = document.body.classList;
// check if we've loaded already from cookie
if (!(classList.contains("dark") || classList.contains("light"))) {
	if (window.matchMedia("(prefers-color-scheme: dark)").matches) {
		classList.add("dark");
	} else {
		classList.remove("dark");
	}
}
