# OFD fixture sources

The checked-in `.ofd` fixtures are test inputs collected from public OFD
projects. Each row below was verified by comparing the local file's SHA-256
with the exact upstream blob at the cited commit. Local filenames were renamed
where useful for the regression test; the OFD bytes were not changed.

The primary verified source is [ofdrw/ofdrw](https://github.com/ofdrw/ofdrw),
which declares an [Apache-2.0 license](https://github.com/ofdrw/ofdrw/blob/master/LICENSE).
The `multi-999.ofd` and `n.ofd` byte sequences are also present in
[DLTech21/ofd.js](https://github.com/DLTech21/ofd.js), whose repository declares
the same license. The duplicate links are included as corroborating public
references, not as a claim about which repository was downloaded first.

| Local fixture | Verified upstream file | Local SHA-256 |
| --- | --- | --- |
| `contains-jpeg.ofd` | [ofdrw `containsJPEG.ofd`](https://github.com/ofdrw/ofdrw/blob/5f854888693fd25617fd8978768e74541bb050be/ofdrw-converter/src/test/resources/containsJPEG.ofd) | `2cb4f2c28b2593a803f7f2516fc100ba6277a48816c4175e8636e0ed25c800bb` |
| `draw-param-ref.ofd` | [ofdrw `draw_param_ref.ofd`](https://github.com/ofdrw/ofdrw/blob/adb3c102fb575387cc9e488ade89f0af832db2de/ofdrw-converter/src/test/resources/draw_param_ref.ofd) | `525eba2e09a4d7ac01a05bc176146ee9ecf5b1184fa3b909cc15599803493700` |
| `helloworld.ofd` | [ofdrw `helloworld.ofd`](https://github.com/ofdrw/ofdrw/blob/6173a9078561b9d8fda8008e061ce92c490c3546/ofdrw-reader/src/test/resources/helloworld.ofd) | `edaf7b227ebff5d7621e05fefc251e572bffa6cb24e9ab1485988b39ba8d4429` |
| `invoice-like.ofd` | [ofdrw `20240531141733.ofd`](https://github.com/ofdrw/ofdrw/blob/c456b19cc1e92bc4857a0563e52b330c60ca3109/ofdrw-converter/src/test/resources/20240531141733.ofd) | `757c576d18e5f2278b42a7a6c99c4e5ec2b4196e1f7ed129db4f614acfccdfdc` |
| `multi-999.ofd` | [ofdrw `999.ofd`](https://github.com/ofdrw/ofdrw/blob/7cb5417266205272f49a9dae5697c5990bbd178e/ofdrw-converter/src/test/resources/999.ofd), also [ofd.js `public/999.ofd`](https://github.com/DLTech21/ofd.js/blob/c426b690dfca3d602f105476639f794479fed1b3/public/999.ofd) | `a2b9080ae35184135160c48ce45fb609c35d5ecfdb538ebc16132c48a2186a80` |
| `n.ofd` | [ofdrw `n.ofd`](https://github.com/ofdrw/ofdrw/blob/b9c13f567027062c5150f4957b350de83147b7ef/ofdrw-converter/src/test/resources/n.ofd), also [ofd.js `public/n.ofd`](https://github.com/DLTech21/ofd.js/blob/c426b690dfca3d602f105476639f794479fed1b3/public/n.ofd) | `306dba9b790fe29af0cd1b365ef259c427319e0ba39c3a5f08dc8eaa2d63147c` |
| `outline-actions.ofd` | [ofdrw `z.ofd`](https://github.com/ofdrw/ofdrw/blob/2e3b367f1ec092ad8be09503e4448c1a256b2c07/ofdrw-converter/src/test/resources/z.ofd) | `ae942157e68402aced7c604d0d17b6421f276cadbab363fa781fea5f948b3596` |
| `pageblock.ofd` | [ofdrw `helloworld_with_pageblock.ofd`](https://github.com/ofdrw/ofdrw/blob/0aa1fa481003c2a5871df16b6355ca01857999eb/ofdrw-reader/src/test/resources/helloworld_with_pageblock.ofd) | `8b122bd9de7647b911189ca99212e07f7db151b58482b22495c35534d129c91e` |
| `path-clip.ofd` | [ofdrw `testPathClip.ofd`](https://github.com/ofdrw/ofdrw/blob/311c673fd48688fbc05cc771feb3f2171e2dee25/ofdrw-converter/src/test/resources/testPathClip.ofd) | `ecfb92f9ab4d394cd574713f5efe2b8836d9ad60de616c043e7b4c702244f87e` |
| `path-fill-opacity.ofd` | [ofdrw `testPathFillOpacity.ofd`](https://github.com/ofdrw/ofdrw/blob/7c267b7f90f96ec23a01fb8217bcf4a6e1052d01/ofdrw-converter/src/test/resources/testPathFillOpacity.ofd) | `054a863d6791867589235fa5b4e7ecd79f13edcae7c0d8f919c74672fcfad7bf` |
| `sample-1.ofd` | [ofdrw `1.ofd`](https://github.com/ofdrw/ofdrw/blob/63af0cf0a58f4533a9d165a5c4f78ab643b20a21/ofdrw-converter/src/test/resources/1.ofd) | `bf41627e626a0cd194c19c37e258f6d9b74b56d8e66541e82f543ae047ab8ad9` |
| `signout.ofd` | [ofdrw `signout.ofd`](https://github.com/ofdrw/ofdrw/blob/b9c13f567027062c5150f4957b350de83147b7ef/ofdrw-converter/src/test/resources/signout.ofd) | `ed321ed91a2b27c875ec60dcab5e409715a3d7d51735bd06616ba14a64866abb` |
| `v4-ride-right.ofd` | [ofdrw `V4RideRight.ofd`](https://github.com/ofdrw/ofdrw/blob/9e22db27b44fd3ec0a2e51dd127288ae2747d3e4/ofdrw-converter/src/test/resources/V4RideRight.ofd) | `3011c7b0c3c73f74433d0068a3e67c3b5d71b077627b2fb54933f0a66eba0473` |
| `watermark-annot.ofd` | [ofdrw `AddWatermarkAnnot.ofd`](https://github.com/ofdrw/ofdrw/blob/12be8f6637bcfd79afc262594c93a59ad4c2ca6b/ofdrw-layout/src/test/resources/AddWatermarkAnnot.ofd) | `dfed483fa08c30d0ee1326cff1b7bd158a5cc29a8dffa9c8425d5a830f21a125` |
| `zsbk.ofd` | [ofdrw `zsbk.ofd`](https://github.com/ofdrw/ofdrw/blob/53c1e6c6ac43d8bf52d568e1f77caeca9895c955/ofdrw-converter/src/test/resources/zsbk.ofd) | `c010c33b10ad91786eaad2de63f9a18ef4a91f96b5e41ba731505fcde9073845` |

The accompanying `*-N.png` files are local golden renderings generated for
this project's regression tests, not upstream files.

Several fixtures contain invoice-like or other realistic document data,
including identifiers, amounts, signatures, and embedded images. This
repository is public because these inputs come from the public upstream test
resources linked above. The upstream code license does not necessarily settle
the rights for every embedded document or image, so preserve this attribution
and review the relevant upstream terms before redistributing or replacing
fixtures and golden renderings.
