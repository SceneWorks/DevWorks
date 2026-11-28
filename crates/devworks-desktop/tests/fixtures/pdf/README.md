# AD65 reproduction fixtures

These are byte-identical copies of the synthetic fixtures and native scan from the recorded
[AD65 PDFium 7881 reproduction](../../../../../docs/architecture/validation/native-boundaries/README.md).
They contain generated test content.

| File | SHA-256 |
| --- | --- |
| mixed.pdf | 2f6cef2078d2ee6b67a3ac72702deca706f5154d8f48eb5833b7e93a07b0d4bc |
| encrypted.pdf | 790df986f1a298babbf041097a285cfaa457333feee37cbed7c433850c120f84 |
| broken.pdf | ddf46a1ce7efaa8401ae63dc957c10448baf937d58a8ee1dd599d7d91cd382eb |
| pdfium-7881-scan.png | dd90eb5b0336adcb8a1d28c514726feb6698697822e490025b3ead28ae266431 |

The 800 × 1,131 grayscale comparison requires dark content in both images, more than 99.9%
bidirectional agreement within one pixel, under 1% difference in the number of dark pixels,
and luma RMSE below 10. Distinct sampling kernels cause subpixel differences. These tests
exercise the app's actual owned worker and perform no inference.
