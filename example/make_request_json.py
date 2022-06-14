#!/usr/bin/env python
import base64
from distutils.file_util import write_file
import json
import subprocess
import os
import hashlib

os.chdir('example_bundle')
subprocess.call(['tar', '-cvf', '../bundle.tar', './'])
os.chdir('../')
subprocess.call(['lz4', '-f', 'bundle.tar', '-12'])
with open('bundle.tar.lz4', 'rb') as fd:
    data = fd.read()
    b64 = base64.b64encode(data).decode('utf-8')
    hash = hashlib.sha512(b64.encode('utf-8')).digest()
    hash_b64 = base64.b64encode(hash).decode('utf-8')

    out = open('bundle.json', 'wb')
    out.write(json.dumps({'task_bundle': b64, 'task_signature': hash_b64}, indent=4, sort_keys=True).encode('utf-8'))
    out.close()
