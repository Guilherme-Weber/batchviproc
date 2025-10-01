# Desktop batch video compression tool

[Download EXE file](https://github.com/Guilherme-Weber/batchviproc/raw/main/batchviproc%201.0.5.exe)

This is a fork that adds OpenCL lookahead acceleration using the GPU and changes the preset from slow to veryslow.
The OpenCL lookahead acceleration speeds things up a little bit and the veryslow preset decrease the filesize.

[Download EXE file with OpenCL](https://github.com/Guilherme-Weber/batchviproc/raw/main/batchviproc%201.0.5-opencl.exe)

<img width="590" alt="batchviproc" src="https://user-images.githubusercontent.com/517919/148660250-5d73c5e8-d1ac-4918-a52e-e26d53000733.png">




## Development

Make sure you have Git, Node JS and NPM installed.

```
git clone https://github.com/Guilherme-Weber/batchviproc.git
cd batchviproc
npm install
npm start
```

# Building EXE file

```
electron-builder --win portable
```

Delete the created `dist` folder and top-level `exe` file before making the next build or the `exe` file size will jump to over 100Mb.
