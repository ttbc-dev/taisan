# taisan hero illustration

`taisan-hero.jpg` is resized from the approved [Issue #44 attachment](https://github.com/user-attachments/assets/a7c9433a-1dc0-42d8-a12d-d503b0604722). The original is 1536 × 1024 pixels. The README asset is 1200 × 800 pixels, with the full 3:2 composition preserved and no added text.

To recreate the asset from a downloaded original:

```sh
sips --resampleHeightWidth 800 1200 -s format jpeg -s formatOptions 82 original.png --out assets/taisan-hero.jpg
```
