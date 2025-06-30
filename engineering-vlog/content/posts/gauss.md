+++
title = "Gaussian Splats with Simple Linear Iterative Clustering"
date = "2024-04-19"

[taxonomies]
tags=["image","shader","Machine Learning","Pixels"]
+++

#### <span style="color:orange;"> Gaussian Splats Using Superpixels</span>


Ever since I stumbled upon ShaderToy, I've been captivated by how some creators integrate complex imagery directly into GLSL shaders without using any external images. The concept of painting with algorithms, particularly using gaussian splats to create intricate effects, intrigued me deeply. It was a challenge I couldn't resist diving into.

My journey began with a desire to understand how to generate these visual elements from scratch. How could one translate a photograph into a format suitable for procedural rendering in shaders in really easy way (so I dont have to spent too much time on a single image)? The answer lay in a fusion of deep learning and traditional image processing techniques.

From Deep Learning to Superpixels:

Last year, I was reading an article about the new superpixel method called "Simple Linear Iterative Clustering" (SLIC), and the authors claim that the method is not only very accurate but its superfast. So I decided to give it a try [source)](https://www.epfl.ch/labs/ivrl/research/slic-superpixels/). I started by segmenting images using a pre-trained DeepLabV3 model, a state-of-the-art tool in semantic image segmentation. This model identifies and isolates various elements of an image, providing a granular breakdown that serves as our foundation. This is really important step. 

To enhance the texture and depth, I incorporated SLIC superpixels. And the good thing is, this method clusters pixels not just based on color but spatial proximity, creating more cohesive and visually appealing segments.


The final touch was alignment. Using PCA (Principal Component Analysis), I thought I could determine the orientation of each segment. By calculating the arctan2 of the principal components, I aligned our gaussian splats precisely, ensuring that each segment not only had the correct position and color but also the correct orientation.

And the all this process took 15 seconds in the Google-Colab, yes without GPU. Here is te output for the 512x512 Lena image:

<div align="center">
<iframe width="640" height="280" frameborder="0" src="https://www.shadertoy.com/embed/4cVGWt?gui=true&t=10&paused=true&muted=false" allowfullscreen></iframe>
</div>

*click to "play" button to see animation*

Note, you can render your own outputs using the my rust backend also, it gives more performance and I included some GUI stuff:

[source rust code for rendering)](https://github.com/altunenes/rusty_art/blob/master/src/gaussiansplat.rs)


here is the python code to rendering your own images:

```python
import torch
import torchvision.transforms as T
from torchvision.models.segmentation import deeplabv3_resnet101
from PIL import Image
import numpy as np
from skimage.segmentation import slic
from skimage import img_as_float, img_as_ubyte
from skimage.color import rgb2gray, label2rgb
from skimage.filters import gaussian, laplace 
from skimage.feature import canny
from sklearn.decomposition import PCA
import matplotlib.pyplot as plt
from sklearn.cluster import KMeans

def load_model():
    model = deeplabv3_resnet101(pretrained=True)
    model.eval()
    return model

def process_image(image_path):
    input_image = Image.open(image_path).convert('RGB')
    preprocess = T.Compose([
        T.Resize((512, 512)), ##  you can decrease, but if you increase you have to adjust pack data fn too
        T.ToTensor(),
        T.Normalize(mean=[0.485, 0.456, 0.406], std=[0.229, 0.224, 0.225])
    ])
    input_tensor = preprocess(input_image).unsqueeze(0)
    return input_tensor, input_image

def segment_image(model, input_tensor, input_image):
    with torch.no_grad():
        output = model(input_tensor)['out'][0]
    semantic_segmentation = output.argmax(0).numpy()
    resized_input_image = input_image.resize((semantic_segmentation.shape[1], semantic_segmentation.shape[0]), Image.LANCZOS)

    gray_image = img_as_float(rgb2gray(np.array(resized_input_image)))
    log_image = laplace(gaussian(gray_image, sigma=1))

    log_image = (log_image - log_image.min()) / (log_image.max() - log_image.min())
    edge_enhanced_image = img_as_ubyte(np.clip(gray_image + log_image, 0, 1))
    # Apply SLIC with refined parameters. If you increase compactness, the output will be more "compact" more "circular", so I suggest decrease it as much as possible for the nice "brush" effect for gaussian splats

    slic_segments = slic(edge_enhanced_image, n_segments=800, compactness=30, sigma=1) 

    image_array = np.array(resized_input_image).reshape((-1, 3))
    n_clusters = min(30, len(np.unique(slic_segments)) // 10)
    kmeans = KMeans(n_clusters=n_clusters).fit(image_array)
    quantized_colors = kmeans.cluster_centers_[kmeans.labels_].reshape(resized_input_image.size[::-1] + (3,))

    combined_segments = slic(quantized_colors, n_segments=800, compactness=30, sigma=1)
    return combined_segments, resized_input_image

def visualize_segmentation(segmentation):
    plt.figure(figsize=(10, 5))
    plt.imshow(label2rgb(segmentation, bg_label=0), interpolation='nearest')
    plt.colorbar()
    plt.title("Segmentation Output")
    plt.show()
def pack_data(x, y, w, h, a, r, g, b): ##if you change image size (on torchvision pipeline), you need to change this function too
    x = clamp(x, 511)  # 9 bits 
    y = clamp(y, 511)  # 9 bits
    w = clamp(w, 255)  # 8 bits
    h = clamp(h, 255)  # 8 bits
    a = clamp(a, 255)  # 8 bits
    r = clamp(r, 255)  # 8 bits
    g = clamp(g, 255)  # 8 bits
    b = clamp(b, 255)  # 8 bits

    xy = (x << 23) | (y << 14)
    whag = (w << 24) | (h << 16) | (a << 8) | g
    rgb = (r << 16) | g | (b << 8)

    return (xy, whag, rgb)

def clamp(value, max_value):
    return int(max(0, min(value, max_value)))


def main():
    model = load_model()
    input_tensor, input_image = process_image('enes.png')  ## of course your image... 
    segmentation, resized_input_image = segment_image(model, input_tensor, input_image)
    visualize_segmentation(segmentation)

    resized_image = np.array(resized_input_image)
    unique_segments = np.unique(segmentation)
    data = []

    for seg_id in unique_segments:
        mask = segmentation == seg_id
        if np.count_nonzero(mask) < 2:
            continue
        segment_coords = np.argwhere(mask)
        if segment_coords.size == 0:
            continue
        y, x = np.mean(segment_coords, axis=0)
        h, w = np.ptp(segment_coords[:, 0]), np.ptp(segment_coords[:, 1])
        color = np.mean(resized_image[mask], axis=0)
        if len(segment_coords) > 1: 
            pca = PCA(n_components=2)
            pca.fit(segment_coords)
            angle = np.arctan2(pca.components_[0, 1], pca.components_[0, 0]) * (180 / np.pi) 
        else:
            angle = 4
        packed_data = pack_data(x, y, w, h, angle, *color.astype(int))
        data.extend(packed_data)

    if data:
        print("const uint data[] = uint[](", end='')
        print(','.join(f"0x{d:08x}u" for d in data), end='')
        print(");")
        print(f"Total data points: {len(data)//3}") 

    else:
        print("No data produced; consider adjusting segment size filter or model parameters.")

if __name__ == '__main__':
    main()
```