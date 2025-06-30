document.addEventListener('DOMContentLoaded', function() {
  document.querySelectorAll('.note-toggle').forEach(function(toggleButton) {
    var content = toggleButton.nextElementSibling;
    var isHidden = content.style.display === 'none';
    toggleButton.setAttribute('aria-expanded', !isHidden);

    toggleButton.addEventListener('click', function() {
      var expanded = this.getAttribute('aria-expanded') === 'true';
      this.setAttribute('aria-expanded', !expanded);
      content.style.display = expanded ? 'none' : 'block';
    });
  });
});

