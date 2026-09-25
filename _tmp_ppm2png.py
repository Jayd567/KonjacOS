import sys
from PIL import Image

src = sys.argv[1]
dst = sys.argv[2]
Image.open(src).save(dst)
