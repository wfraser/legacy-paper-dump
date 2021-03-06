This program will produce a dump of all legacy paper docs you have opened.

To use it, you need a Dropbox API key. Go to https://www.dropbox.com/developers/apps and create
yourself a development app if you don't have one already. Choose Dropbox Legacy API, and then the
Full Dropbox access. Scoped access *might* work but I haven't tested it with these endpoints.

Once you created your app, go to the app's page in the dashboard, find `Generated access token` and
click the `Generate` button.

Then in your shell, type `export DBX_OAUTH_TOKEN=<that string>`.

Then run `cargo run | tee output.txt` and it'll do its thing. Most of the time is spent fetching
metadata for the docs so it can give you titles, which has to be done one doc at a time...

It'll warn about deprecated functions, because we're using the legacy Paper API which is, in fact,
deprecated.
