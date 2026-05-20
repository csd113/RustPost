# Changelog

## v0.1.3

### Search
- Search now has a cleaner page layout and clearer result cards.
- It handles posts, usernames, mentions, and hashtags more consistently.
- Bad or unusual search input no longer sends people to an error page.
- Search results can be liked or opened without leaving the search flow.

### Profiles and Media
- Profile pictures now use thumbnails in compact places, which keeps feeds faster and cleaner.
- Full-size profile pages still show the original image.
- The database now tracks thumbnail paths for stored media.

### Admin
- The admin media jobs view is shorter and easier to read.
- It now shows the most useful job status details and recent failures at a glance.
- Admins can upload a custom favicon and reset back to the built-in one.

### Login and Registration
- Login now shows specific errors for missing accounts, wrong passwords, and unavailable accounts.
- Registration now shows a clear message when a username is already taken.

### Threads
- Thread pages no longer show an extra generic thread header.
- The root post no longer links back to itself, which makes navigation less confusing.
