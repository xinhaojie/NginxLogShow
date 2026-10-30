use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
use base64::{engine::general_purpose::STANDARD, Engine};
use rand::rng;
use rand::Rng;

const DEFAULT_KEY: &str = "7sK9tG5pR2xQ8bN3dF6jH4aZ1cV0mW7L";
const BLOCK_SIZE: usize = 16;
const KEY_SIZE: usize = 32;

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

pub struct AesCrypto {
    key: [u8; 32],
}

impl AesCrypto {
    pub fn new(secret_key: Option<&str>) -> Result<Self, String> {
        let key_str = secret_key.unwrap_or(DEFAULT_KEY);
        
        if key_str.len() != KEY_SIZE {
            return Err(format!("密钥必须为{}位字符串(AES-256)", KEY_SIZE));
        }
        
        let mut key = [0u8; 32];
        key.copy_from_slice(key_str.as_bytes());
        
        Ok(Self { key })
    }
    
    pub fn encrypt(&self, plain_text: &str) -> Result<String, String> {
        let mut iv = [0u8; 16];
        rng().fill_bytes(&mut iv);
        
        let cipher = Aes256CbcEnc::new_from_slices(&self.key, &iv)
            .map_err(|e| format!("初始化加密器失败: {}", e))?;
        
        let data_bytes = plain_text.as_bytes();
        let mut buffer = vec![0u8; data_bytes.len() + BLOCK_SIZE];
        buffer[..data_bytes.len()].copy_from_slice(data_bytes);
        
        let ct = cipher
            .encrypt_padded::<Pkcs7>(&mut buffer, data_bytes.len())
            .map_err(|e| format!("加密失败: {}", e))?;

        let result = [iv.as_slice(), ct].concat();
        Ok(STANDARD.encode(&result))
    }
    
    pub fn decrypt(&self, cipher_text: &str) -> Result<String, String> {
        let raw_bytes = STANDARD
            .decode(cipher_text)
            .map_err(|e| format!("Base64解码失败: {}", e))?;
        
        if raw_bytes.len() < BLOCK_SIZE {
            return Err("密文长度不足".to_string());
        }
        
        let iv: [u8; 16] = raw_bytes[..BLOCK_SIZE]
            .try_into()
            .map_err(|_| "IV提取失败".to_string())?;
        let encrypted_bytes = &raw_bytes[BLOCK_SIZE..];
        
        let cipher = Aes256CbcDec::new_from_slices(&self.key, &iv)
            .map_err(|e| format!("初始化解密器失败: {}", e))?;
        
        let mut buffer = encrypted_bytes.to_vec();
        let pt = cipher
            .decrypt_padded::<Pkcs7>(&mut buffer)
            .map_err(|e| format!("解密失败: {}", e))?;

        String::from_utf8(pt.to_vec())
            .map_err(|e| format!("UTF-8解码失败: {}", e))
    }
}

lazy_static::lazy_static! {
    pub static ref AES_CRYPTO: AesCrypto = AesCrypto::new(None).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_encrypt_decrypt() {
        let plain_text = "Hello, 世界!";
        let encrypted = AES_CRYPTO.encrypt(plain_text).unwrap();
        let decrypted = AES_CRYPTO.decrypt(&encrypted).unwrap();
        assert_eq!(plain_text, decrypted);
    }
    
    #[test]
    fn test_custom_key() {
        let custom_key = "12345678901234567890123456789012";
        let crypto = AesCrypto::new(Some(custom_key)).unwrap();
        
        let plain_text = "Test message";
        let encrypted = crypto.encrypt(plain_text).unwrap();
        let decrypted = crypto.decrypt(&encrypted).unwrap();
        assert_eq!(plain_text, decrypted);
    }
    
    #[test]
    fn test_invalid_key_length() {
        let result = AesCrypto::new(Some("short"));
        assert!(result.is_err());
    }
}