import zipfile, os, shutil

src = "C:/Users/69497/flutter_win.zip"
work = "C:/Users/69497/WorkBuddy/remote-desktop"
tmp = os.path.join(work, ".flsdk_tmp")
dest = os.path.join(work, "flutter_sdk")

if os.path.isdir(tmp):
    shutil.rmtree(tmp)
if os.path.isdir(dest):
    shutil.rmtree(dest)
os.makedirs(tmp)

print("extracting...")
with zipfile.ZipFile(src) as z:
    z.extractall(tmp)

inner = os.path.join(tmp, "flutter")
assert os.path.isdir(inner), "expected flutter/ inside zip"
os.rename(inner, dest)
shutil.rmtree(tmp)

print("flutter.bat exists:", os.path.isfile(os.path.join(dest, "bin", "flutter.bat")))
print("DONE")
