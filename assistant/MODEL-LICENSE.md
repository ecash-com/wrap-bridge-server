# Model license

The assistant runs **Llama 3.2 3B Instruct**, built with Llama. Llama 3.2 is licensed under the
Llama 3.2 Community License, Copyright © Meta Platforms, Inc. All Rights Reserved.

Redistributing the model file with this project is permitted by that license on these terms,
which this project follows:

- The notice "Built with Llama" appears here and in the assistant's own description.
- A copy of the license and the Acceptable Use Policy accompanies the model. Both are published by
  Meta at https://www.llama.com/llama3_2/license/ and https://www.llama.com/llama3_2/use-policy/,
  and `make-bundle.sh` places copies in `bundle/model/` next to the weights.
- The model is used only to read this bridge's own status and logs for its operator. It is not
  exposed to the public and is not used to improve any other model.

The quantized file is the 4-bit `Q4_K_M` GGUF conversion published by bartowski on Hugging Face,
unmodified; its SHA-256 is pinned in `MODEL.conf`.

The license is Meta's, not this project's. Operators who redistribute the bundle themselves take on
the same obligations.
