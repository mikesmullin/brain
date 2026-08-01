// app.js — HMR boot entry. Called by index.html on load and by m.js's
// hot-client on every change push, with a cache-bust timestamp so edited
// modules re-import fresh. ui first (creates the named store), scene second
// (reads it and installs its action API).
export async function boot(t = 0) {
  const ui = await import('/ui.js?t=' + t)
  const scene = await import('/scene.js?t=' + t)
  await ui.boot()
  await scene.boot()
}
